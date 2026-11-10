//! Versioned on-disk formats: one [`Format`] descriptor per file cactup
//! writes and reads back, and the one read and write path they all share.
//! The plan is `design/format-versioning-plan.md` §2.
//!
//! - **The version belongs to the file, not to the value.** Structs carry no
//!   `schema` field: [`read`] strips the version key before the typed parse
//!   and [`write`] puts it back, first.
//! - **Read: version first.** The raw document is parsed, its version key
//!   read (missing = 1), and a version newer than [`Format::current`] is
//!   refused with one upgrade message naming the file ([`Newer`]) before any
//!   typed parse. An older one runs [`Format::legacy`] and the upgrade chain
//!   over the raw document, then parses. A read never rewrites the file.
//! - **Write: atomic, never over a newer file, sticky, keeping what it does
//!   not know.** The target is re-read first; a newer version is refused. The
//!   version written is the oldest one, no older than the file on disk, that
//!   the down-chain reaches (with no down-steps: `current`). Keys on disk
//!   that this binary does not model are put back (maps only, only where
//!   absent), so an older binary's rewrite keeps a newer one's additions.

use anyhow::Context;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::Res;

/// The raw document of a JSON format (`database.json`).
pub type Json = serde_json::Map<String, serde_json::Value>;
/// The raw document of a TOML format.
pub type Toml = toml::Table;

/// An upgrade: v(N) to v(N+1), in place.
pub type Upgrade<D> = fn(&mut D) -> Res<()>;
/// A down-step: v(N+1) back to v(N), in place.
pub type Downgrade<D> = fn(&mut D) -> Result<(), CannotExpress>;

/// A down-step's answer when the document uses something the older version
/// cannot hold: the write then stays at the newer version.
#[derive(Debug)]
pub struct CannotExpress;

/// One versioned format. A `static` in the module that owns the record.
pub struct Format<D: 'static> {
    /// What the file is, for messages: "database", "restart record".
    pub name: &'static str,
    /// The version key: `schema` for local records, `format` for the shared
    /// crate's interchange formats.
    pub key: &'static str,
    /// The newest version this binary reads and writes.
    pub current: u32,
    /// `[i]` upgrades v(i+1) to v(i+2) in place, so `len == current - 1`.
    /// Kept forever.
    pub upgrades: &'static [Upgrade<D>],
    /// `[i]` turns v(down_floor+i+1) back into v(down_floor+i), or answers
    /// [`CannotExpress`]. Either empty or complete
    /// (`len == current - down_floor`).
    pub downgrades: &'static [Downgrade<D>],
    /// The oldest version the down-chain reaches; nothing is written older.
    pub down_floor: u32,
    /// Read by a queued job's frozen binary, so every breaking bump must
    /// ship its down-step (the generic tests enforce it).
    // Read only by the generic tests (test 5).
    #[cfg_attr(not(test), allow(dead_code))]
    pub frozen_read: bool,
    /// Same-version normalizations shipped before versioning did their job,
    /// run before the upgrades. A closed list: new changes bump instead.
    pub legacy: Option<fn(&mut D)>,
}

/// A file written by a newer cactup: refused on read and on write.
#[derive(Debug)]
pub struct Newer {
    pub path: PathBuf,
    pub name: &'static str,
    pub key: &'static str,
    pub found: u64,
    pub current: u32,
}

impl std::fmt::Display for Newer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // One message for every format; no spec references (users read it).
        write!(
            f,
            "'{}' was written by a newer cactup ({} {} {}; this cactup, {}, reads up to {}); upgrade cactup",
            self.path.display(),
            self.name,
            self.key,
            self.found,
            crate::VERSION,
            self.current
        )
    }
}

impl std::error::Error for Newer {}

/// The raw document did not parse: the one failure a lock-free reader may
/// retry, since only a torn or half-visible file produces it.
#[derive(Debug)]
pub struct Unparsable {
    pub path: PathBuf,
    pub name: &'static str,
    pub detail: String,
}

impl std::fmt::Display for Unparsable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Failed to parse {} at '{}': {}", self.name, self.path.display(), self.detail)
    }
}

impl std::error::Error for Unparsable {}

/// Did this error come from a raw document that would not parse?
pub fn is_unparsable(err: &anyhow::Error) -> bool {
    err.downcast_ref::<Unparsable>().is_some()
}

/// The raw document a format is read and written through: a string-keyed
/// map that keeps its key order (`toml` and `serde_json` both built with
/// `preserve_order`).
pub trait Document: Clone + Default + PartialEq + std::fmt::Debug {
    type Value: Clone + PartialEq;
    fn parse(text: &str) -> Result<Self, String>;
    fn render(&self) -> Res<String>;
    fn from_typed<T: Serialize>(value: &T) -> Res<Self>;
    fn into_typed<T: DeserializeOwned>(self) -> Res<T>;
    fn get(&self, key: &str) -> Option<&Self::Value>;
    fn get_mut(&mut self, key: &str) -> Option<&mut Self::Value>;
    // Upgrades and tests use these; no migration does yet.
    #[cfg_attr(not(test), allow(dead_code))]
    fn insert(&mut self, key: String, value: Self::Value);
    /// Remove `key`, keeping the order of the rest.
    fn remove(&mut self, key: &str) -> Option<Self::Value>;
    fn iter(&self) -> impl Iterator<Item = (&String, &Self::Value)>;
    fn into_entries(self) -> Vec<(String, Self::Value)>;
    fn from_entries(entries: Vec<(String, Self::Value)>) -> Self;
    fn as_map(value: &Self::Value) -> Option<&Self>;
    fn as_map_mut(value: &mut Self::Value) -> Option<&mut Self>;
    #[cfg_attr(not(test), allow(dead_code))]
    fn into_value(map: Self) -> Self::Value;
    /// The value as a version, if it is a non-negative integer.
    fn as_version(value: &Self::Value) -> Option<u64>;
    fn version_value(version: u32) -> Self::Value;
}

impl Document for Toml {
    type Value = toml::Value;
    fn parse(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }
    fn render(&self) -> Res<String> {
        toml::to_string_pretty(self).context("Failed to serialize TOML")
    }
    fn from_typed<T: Serialize>(value: &T) -> Res<Self> {
        match toml::Value::try_from(value)? {
            toml::Value::Table(table) => Ok(table),
            other => anyhow::bail!("serialized to a TOML {}, not a table", other.type_str()),
        }
    }
    fn into_typed<T: DeserializeOwned>(self) -> Res<T> {
        Ok(toml::Value::Table(self).try_into()?)
    }
    fn get(&self, key: &str) -> Option<&Self::Value> {
        toml::Table::get(self, key)
    }
    fn get_mut(&mut self, key: &str) -> Option<&mut Self::Value> {
        toml::Table::get_mut(self, key)
    }
    fn insert(&mut self, key: String, value: Self::Value) {
        toml::Table::insert(self, key, value);
    }
    fn remove(&mut self, key: &str) -> Option<Self::Value> {
        // `preserve_order` makes this a shift-remove.
        toml::Table::remove(self, key)
    }
    fn iter(&self) -> impl Iterator<Item = (&String, &Self::Value)> {
        toml::Table::iter(self)
    }
    fn into_entries(self) -> Vec<(String, Self::Value)> {
        self.into_iter().collect()
    }
    fn from_entries(entries: Vec<(String, Self::Value)>) -> Self {
        entries.into_iter().collect()
    }
    fn as_map(value: &Self::Value) -> Option<&Self> {
        value.as_table()
    }
    fn as_map_mut(value: &mut Self::Value) -> Option<&mut Self> {
        value.as_table_mut()
    }
    fn into_value(map: Self) -> Self::Value {
        toml::Value::Table(map)
    }
    fn as_version(value: &Self::Value) -> Option<u64> {
        value.as_integer().and_then(|v| u64::try_from(v).ok())
    }
    fn version_value(version: u32) -> Self::Value {
        toml::Value::Integer(i64::from(version))
    }
}

impl Document for Json {
    type Value = serde_json::Value;
    fn parse(text: &str) -> Result<Self, String> {
        serde_json::from_str(text).map_err(|e| e.to_string())
    }
    fn render(&self) -> Res<String> {
        serde_json::to_string_pretty(self).context("Failed to serialize JSON")
    }
    fn from_typed<T: Serialize>(value: &T) -> Res<Self> {
        match serde_json::to_value(value)? {
            serde_json::Value::Object(map) => Ok(map),
            other => anyhow::bail!("serialized to JSON {other}, not an object"),
        }
    }
    fn into_typed<T: DeserializeOwned>(self) -> Res<T> {
        Ok(serde_json::from_value(serde_json::Value::Object(self))?)
    }
    fn get(&self, key: &str) -> Option<&Self::Value> {
        serde_json::Map::get(self, key)
    }
    fn get_mut(&mut self, key: &str) -> Option<&mut Self::Value> {
        serde_json::Map::get_mut(self, key)
    }
    fn insert(&mut self, key: String, value: Self::Value) {
        serde_json::Map::insert(self, key, value);
    }
    fn remove(&mut self, key: &str) -> Option<Self::Value> {
        self.shift_remove(key)
    }
    fn iter(&self) -> impl Iterator<Item = (&String, &Self::Value)> {
        serde_json::Map::iter(self)
    }
    fn into_entries(self) -> Vec<(String, Self::Value)> {
        self.into_iter().collect()
    }
    fn from_entries(entries: Vec<(String, Self::Value)>) -> Self {
        entries.into_iter().collect()
    }
    fn as_map(value: &Self::Value) -> Option<&Self> {
        value.as_object()
    }
    fn as_map_mut(value: &mut Self::Value) -> Option<&mut Self> {
        value.as_object_mut()
    }
    fn into_value(map: Self) -> Self::Value {
        serde_json::Value::Object(map)
    }
    fn as_version(value: &Self::Value) -> Option<u64> {
        value.as_u64()
    }
    fn version_value(version: u32) -> Self::Value {
        serde_json::Value::from(version)
    }
}

/// What a write found at its target, read before it writes.
pub struct OnDisk<D> {
    state: DiskState<D>,
}

enum DiskState<D> {
    Absent,
    /// Present but unusable (the raw document, its version key, or an upgrade
    /// failed): overwritten with nothing put back.
    // Only `write` finds it; the TOML records start using `write` in P2a.
    #[cfg_attr(not(test), allow(dead_code))]
    Unusable,
    /// The file's version and its document after `legacy` and the upgrades,
    /// version key stripped.
    Parsed {
        version: u32,
        doc: D,
    },
}

/// Read `path` as `f`; `None` when there is no file.
pub fn read<T: DeserializeOwned, D: Document>(path: &Path, f: &Format<D>) -> Res<Option<T>> {
    match read_text(path, f)? {
        None => Ok(None),
        Some(text) => parse(&text, path, f).map(Some),
    }
}

/// Parse the text of `path` (already read) as `f`. A newer file fails with
/// [`Newer`], a raw document that does not parse with [`Unparsable`].
pub fn parse<T: DeserializeOwned, D: Document>(text: &str, path: &Path, f: &Format<D>) -> Res<T> {
    let (_, doc) = decode(text, path, f)?;
    typed(doc, path, f)
}

/// [`read`], keeping what the later [`write_over`] of the same file needs, so
/// a read-modify-write under a lock reads the file once.
pub fn read_for_write<T: DeserializeOwned, D: Document>(
    path: &Path,
    f: &Format<D>,
) -> Res<(Option<T>, OnDisk<D>)> {
    let Some(text) = read_text(path, f)? else {
        return Ok((None, OnDisk { state: DiskState::Absent }));
    };
    let (version, doc) = decode(text.as_str(), path, f)?;
    let value = typed(doc.clone(), path, f)?;
    Ok((Some(value), OnDisk { state: DiskState::Parsed { version, doc } }))
}

/// Write `value` to `path` as `f`: re-read the target, then [`write_over`].
#[cfg_attr(not(test), allow(dead_code))] // the TOML records use it from P2a
pub fn write<T: Serialize + DeserializeOwned, D: Document>(
    path: &Path,
    value: &T,
    f: &Format<D>,
) -> Res<()> {
    let state = match read_text(path, f)? {
        None => DiskState::Absent,
        Some(text) => match decode(&text, path, f) {
            Ok((version, doc)) => DiskState::Parsed { version, doc },
            // Never overwrite a newer file.
            Err(e) if e.downcast_ref::<Newer>().is_some() => return Err(e),
            Err(_) => DiskState::Unusable,
        },
    };
    write_over(path, value, f, &OnDisk { state })
}

/// Write `value` over what [`read_for_write`] (or [`write`]) found at `path`.
pub fn write_over<T: Serialize + DeserializeOwned, D: Document>(
    path: &Path,
    value: &T,
    f: &Format<D>,
    disk: &OnDisk<D>,
) -> Res<()> {
    let mut doc = D::from_typed(value).with_context(|| format!("Failed to serialize {}", f.name))?;
    // The helper owns the version key, whatever the struct says.
    doc.remove(f.key);
    let floor = match &disk.state {
        DiskState::Parsed { version, doc: on_disk } => {
            if *version > f.current {
                return Err(newer(path, f, u64::from(*version)));
            }
            // Put back before stepping down: the disk document (upgraded),
            // its round trip and `doc` are all at `current` here, so the
            // comparison never mixes two versions' key names; a down-step
            // leaves keys it does not know alone.
            if let Some(known) = round_trip::<T, D>(on_disk, f) {
                put_back(&mut doc, on_disk, &known);
            }
            *version
        }
        DiskState::Absent | DiskState::Unusable => f.down_floor,
    };
    let (version, doc) = f.express(doc, floor);
    write_atomic(path, &with_version_first(doc, f.key, version).render()?)
}

/// Atomic write (temp file in the same directory + fsync + rename), so a
/// reader sees the old file or the new one, never a torn one.
pub(crate) fn write_atomic(path: &Path, text: &str) -> Res<()> {
    let dir = path.parent().expect("cactup record paths have parents");
    std::fs::create_dir_all(dir).with_context(|| format!("Failed to create '{}'", dir.display()))?;
    let mut temp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("Failed to create temp file in '{}'", dir.display()))?;
    temp.write_all(text.as_bytes())
        .and_then(|()| temp.as_file().sync_all())
        .with_context(|| format!("Failed to write '{}'", path.display()))?;
    temp.persist(path)
        .with_context(|| format!("Failed to move '{}' into place", path.display()))?;
    Ok(())
}

impl<D: Document> Format<D> {
    /// Step `doc` (at `current`) down the chain as far as it goes without
    /// reaching below `floor`: the version to write, and the document at it.
    fn express(&self, doc: D, floor: u32) -> (u32, D) {
        let floor = floor.max(self.down_floor);
        let (mut version, mut doc) = (self.current, doc);
        while version > floor {
            let index = (version - self.down_floor - 1) as usize;
            let Some(step) = self.downgrades.get(index) else { break };
            let mut older = doc.clone();
            match step(&mut older) {
                Ok(()) => {
                    doc = older;
                    version -= 1;
                }
                Err(CannotExpress) => break,
            }
        }
        (version, doc)
    }
}

fn read_text<D>(path: &Path, f: &Format<D>) -> Res<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("Failed to read {} at '{}'", f.name, path.display())),
    }
}

fn newer<D>(path: &Path, f: &Format<D>, found: u64) -> anyhow::Error {
    anyhow::Error::new(Newer {
        path: path.to_owned(),
        name: f.name,
        key: f.key,
        found,
        current: f.current,
    })
}

/// Raw parse → version → refuse newer → strip the key → `legacy` → upgrades.
fn decode<D: Document>(text: &str, path: &Path, f: &Format<D>) -> Res<(u32, D)> {
    let mut doc = D::parse(text).map_err(|detail| {
        anyhow::Error::new(Unparsable { path: path.to_owned(), name: f.name, detail })
    })?;
    let version = match doc.get(f.key) {
        None => 1,
        Some(value) => match D::as_version(value) {
            Some(v) if v > u64::from(f.current) => return Err(newer(path, f, v)),
            Some(v) if v >= 1 => v as u32,
            _ => anyhow::bail!(
                "Failed to parse {} at '{}': `{}` is not a positive integer",
                f.name,
                path.display(),
                f.key
            ),
        },
    };
    doc.remove(f.key);
    if let Some(legacy) = f.legacy {
        legacy(&mut doc);
    }
    for (index, upgrade) in f.upgrades.iter().enumerate().skip(version as usize - 1) {
        upgrade(&mut doc).with_context(|| {
            format!(
                "Failed to upgrade {} at '{}' from {} {} to {}",
                f.name,
                path.display(),
                f.key,
                index + 1,
                index + 2
            )
        })?;
    }
    Ok((version, doc))
}

fn typed<T: DeserializeOwned, D: Document>(doc: D, path: &Path, f: &Format<D>) -> Res<T> {
    doc.into_typed()
        .with_context(|| format!("Failed to parse {} at '{}'", f.name, path.display()))
}

/// The on-disk document as this binary models it: typed-parsed and
/// serialized again. `None` when it does not parse (nothing is put back).
fn round_trip<T: Serialize + DeserializeOwned, D: Document>(on_disk: &D, f: &Format<D>) -> Option<D> {
    let value: T = on_disk.clone().into_typed().ok()?;
    let mut known = D::from_typed(&value).ok()?;
    known.remove(f.key);
    Some(known)
}

/// Copy into `new` every key of `on_disk` that `known` (the same document
/// after a typed round trip) lacks, only where `new` lacks it too; recurse
/// into keys that are maps in all three. Never into arrays: an element's
/// identity is its position, and a reordered array would graft a key onto
/// the wrong element. A key the writer cleared is in `known`, so it stays
/// cleared. A put-back key goes after the nearest key that precedes it on
/// disk and is present in `new`, so the file keeps its order.
fn put_back<D: Document>(new: &mut D, on_disk: &D, known: &D) {
    let mut anchor: Option<&str> = None;
    for (key, disk_value) in on_disk.iter() {
        match known.get(key) {
            None if new.get(key).is_none() => insert_after(new, anchor, key.clone(), disk_value.clone()),
            None => {}
            Some(known_value) => {
                if let (Some(disk_map), Some(known_map), Some(new_map)) = (
                    D::as_map(disk_value),
                    D::as_map(known_value),
                    new.get_mut(key).and_then(D::as_map_mut),
                ) {
                    put_back(new_map, disk_map, known_map);
                }
            }
        }
        if new.get(key).is_some() {
            anchor = Some(key);
        }
    }
}

fn insert_after<D: Document>(map: &mut D, anchor: Option<&str>, key: String, value: D::Value) {
    let mut entries = std::mem::take(map).into_entries();
    let at = anchor
        .and_then(|a| entries.iter().position(|(k, _)| k == a))
        .map_or(0, |i| i + 1);
    entries.insert(at, (key, value));
    *map = D::from_entries(entries);
}

fn with_version_first<D: Document>(doc: D, key: &str, version: u32) -> D {
    let mut entries = doc.into_entries();
    entries.retain(|(k, _)| k != key);
    entries.insert(0, (key.to_owned(), D::version_value(version)));
    D::from_entries(entries)
}

/// The generic tests every format runs (plan §2.5): each format's module
/// calls these on its own descriptor, fixtures and sample.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// `fixtures/formats/<format>/` in the source tree.
    pub fn fixture_dir(format: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/formats").join(format)
    }

    fn version_of<D: Document>(text: &str, f: &Format<D>) -> u64 {
        let doc = D::parse(text).unwrap();
        doc.get(f.key).map_or(1, |v| D::as_version(v).unwrap())
    }

    /// The descriptor is self-consistent, and (test 5) a `frozen_read`
    /// format has every down-step.
    pub fn descriptor_is_consistent<D: Document>(f: &Format<D>) {
        assert!(f.current >= 1, "{}: current must be at least 1", f.name);
        assert_eq!(f.upgrades.len() as u32, f.current - 1, "{}: one upgrade per version step", f.name);
        assert!((1..=f.current).contains(&f.down_floor), "{}: down_floor out of range", f.name);
        let complete = (f.current - f.down_floor) as usize;
        assert!(
            f.downgrades.is_empty() || f.downgrades.len() == complete,
            "{}: the down-chain must be empty or reach down_floor",
            f.name
        );
        if f.frozen_read {
            assert_eq!(
                f.downgrades.len(),
                complete,
                "{} is read by queued jobs' frozen binaries: every breaking bump ships its \
                 v(N+1) -> vN down-step",
                f.name
            );
        }
    }

    /// Test 1: every fixture loads; written back over a copy of itself it
    /// reads back to the same value, at the version the write rule picks.
    pub fn every_fixture_loads<T: Serialize + DeserializeOwned, D: Document>(f: &Format<D>, dir: &Path) {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert!(
            names.iter().any(|n| n.starts_with("current.")),
            "{}: no current fixture",
            dir.display()
        );
        assert!(names.iter().any(|n| n.starts_with("v1")), "{}: no v1 fixture", dir.display());
        let scratch = tempfile::tempdir().unwrap();
        for name in names {
            let text = std::fs::read_to_string(dir.join(&name)).unwrap();
            let path = scratch.path().join(&name);
            std::fs::write(&path, &text).unwrap();
            let first: T = read(&path, f).unwrap_or_else(|e| panic!("{name}: {e:#}")).unwrap();
            write(&path, &first, f).unwrap_or_else(|e| panic!("{name}: {e:#}"));
            let written = std::fs::read_to_string(&path).unwrap();
            let second: T =
                read(&path, f).unwrap_or_else(|e| panic!("{name} written back: {e:#}")).unwrap();
            assert_eq!(D::from_typed(&first).unwrap(), D::from_typed(&second).unwrap(), "{name}");

            let on_disk = version_of(&text, f) as u32;
            let version = version_of(&written, f) as u32;
            let mut doc = D::from_typed(&first).unwrap();
            doc.remove(f.key);
            assert_eq!(version, f.express(doc, on_disk).0, "{name}: version written");
            assert!(
                (on_disk.max(f.down_floor)..=f.current).contains(&version),
                "{name}: version {version}"
            );
            if f.downgrades.is_empty() {
                assert_eq!(version, f.current, "{name}");
            }
        }
    }

    /// Test 2, the shape pin: the exhaustive sample (a struct literal without
    /// `..`, every `Option` `Some`, every collection non-empty) serializes to
    /// exactly `current.*`, compared as documents, not bytes.
    pub fn shape_is_pinned<T: Serialize, D: Document>(f: &Format<D>, dir: &Path, sample: &T) {
        let name = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .find(|n| n.starts_with("current."))
            .unwrap_or_else(|| panic!("{}: no current fixture", dir.display()));
        let pinned = D::parse(&std::fs::read_to_string(dir.join(&name)).unwrap()).unwrap();
        let mut doc = D::from_typed(sample).unwrap();
        doc.remove(f.key);
        let sample = with_version_first(doc, f.key, f.current);
        let ext = name.trim_start_matches("current.");
        assert!(
            sample == pinned,
            "the {} shape changed: the exhaustive sample no longer matches {}/{name}.\n\
             First keep the shape that shipped: copy the old {name} to a new v{}-<k>.{ext} (k: the \
             next free number, or a word saying what it is). Then, for an additive change (a new \
             optional key whose absence means the old behavior and that an older binary may \
             ignore), re-pin {name} to the text below. For anything else, bump `current` to {}, \
             add the upgrade (and the down-step if the format is frozen_read), and re-pin {name} \
             to the text below with the new version.\n\
             The sample now serializes to:\n{}",
            f.name,
            dir.display(),
            f.current,
            f.current + 1,
            sample.render().unwrap()
        );
    }

    /// Test 3: a newer file is refused before the typed parse (the body is
    /// invalid for the typed struct), by name, by read and by write alike,
    /// and the file is left byte for byte as it was.
    pub fn newer_is_refused<T: Serialize + DeserializeOwned, D: Document>(
        f: &Format<D>,
        file_name: &str,
        invalid_body: &str,
        sample: &T,
    ) {
        let body = D::parse(invalid_body).unwrap();
        assert!(
            body.clone().into_typed::<T>().is_err(),
            "the test body must not parse as the typed struct"
        );
        let text = with_version_first(body, f.key, f.current + 1).render().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(file_name);
        std::fs::write(&path, &text).unwrap();

        let Err(err) = read::<T, D>(&path, f) else { panic!("a newer {} was read", f.name) };
        let message = format!("{err:#}");
        assert!(message.contains(&format!("'{}'", path.display())), "{message}");
        assert!(message.contains("upgrade cactup") && !message.contains('§'), "{message}");
        assert!(message.contains(&format!("reads up to {}", f.current)), "{message}");
        let err = write(&path, sample, f).unwrap_err();
        assert!(err.downcast_ref::<Newer>().is_some(), "{err:#}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(names.len(), 1, "no temp file is left behind");
    }

    /// Follow a path of map keys.
    fn at<'a, D: Document>(doc: &'a mut D, path: &[&str]) -> &'a mut D {
        path.iter().fold(doc, |map, key| {
            map.get_mut(key)
                .and_then(D::as_map_mut)
                .unwrap_or_else(|| panic!("no map at {key}"))
        })
    }

    /// Test 4: keys this binary does not model survive a write, at the top
    /// level and in the map at `nested`; the known key at `cleared`, which
    /// `clear` empties, stays cleared; the version key comes first.
    pub fn unknown_keys_survive<T: Serialize + DeserializeOwned, D: Document>(
        f: &Format<D>,
        file_name: &str,
        sample: &T,
        nested: &[&str],
        clear: impl FnOnce(&mut T),
        cleared: &[&str],
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(file_name);
        write(&path, sample, f).unwrap();
        let mut doc = D::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let future = D::into_value(D::from_entries(vec![("x".to_owned(), D::version_value(1))]));
        doc.insert("cactup-future".to_owned(), future.clone());
        at(&mut doc, nested).insert("cactup-future-field".to_owned(), D::version_value(2));
        std::fs::write(&path, doc.render().unwrap()).unwrap();

        let mut value: T = read(&path, f).unwrap().unwrap();
        clear(&mut value);
        write(&path, &value, f).unwrap();

        let mut doc = D::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            doc.iter().next().map(|(k, _)| k.as_str()),
            Some(f.key),
            "the version key comes first"
        );
        assert!(doc.get("cactup-future") == Some(&future), "top-level unknown key lost: {doc:?}");
        assert!(
            at(&mut doc, nested).get("cactup-future-field") == Some(&D::version_value(2)),
            "nested unknown key lost"
        );
        let (last, parent) = cleared.split_last().unwrap();
        assert!(at(&mut doc, parent).get(last).is_none(), "a cleared key came back");
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use serde_derive::{Deserialize, Serialize};

    /// A toy record at version 2: v1 called `name` `title`, and v2 added
    /// `color`, which a v1 binary would get wrong by ignoring it.
    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "kebab-case")]
    struct Toy {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        finished: Option<Finished>,
        #[serde(default)]
        sub: Sub,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        items: Vec<Sub>,
    }

    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    struct Finished {
        code: i64,
    }

    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    struct Sub {
        #[serde(default)]
        n: i64,
    }

    fn rename(doc: &mut Toml, from: &str, to: &str) {
        if let Some(v) = doc.remove(from) {
            doc.insert(to.to_owned(), v);
        }
    }

    fn toy_up_1(doc: &mut Toml) -> Res<()> {
        rename(doc, "title", "name");
        Ok(())
    }

    fn toy_down_2(doc: &mut Toml) -> Result<(), CannotExpress> {
        if doc.contains_key("color") {
            return Err(CannotExpress);
        }
        rename(doc, "name", "title");
        Ok(())
    }

    fn toy_legacy(doc: &mut Toml) {
        doc.remove("obsolete");
    }

    static TOY: Format<Toml> = Format {
        name: "toy record",
        key: "schema",
        current: 2,
        upgrades: &[toy_up_1],
        downgrades: &[toy_down_2],
        down_floor: 1,
        frozen_read: true,
        legacy: Some(toy_legacy),
    };

    fn sample() -> Toy {
        Toy {
            name: "a".into(),
            color: Some("red".into()),
            finished: Some(Finished { code: 0 }),
            sub: Sub { n: 1 },
            items: vec![Sub { n: 2 }],
        }
    }

    const CURRENT: &str = "schema = 2\nname = \"a\"\ncolor = \"red\"\n\n[finished]\ncode = 0\n\n\
                           [sub]\nn = 1\n\n[[items]]\nn = 2\n";

    fn toy_fixtures() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in [
            ("v1-noschema.toml", "title = \"a\"\nobsolete = true\n"),
            ("v1.toml", "schema = 1\ntitle = \"a\"\n\n[sub]\nn = 3\n"),
            ("current.toml", CURRENT),
        ] {
            std::fs::write(dir.path().join(name), text).unwrap();
        }
        dir
    }

    #[test]
    fn toy_generic_tests() {
        let fixtures = toy_fixtures();
        descriptor_is_consistent(&TOY);
        shape_is_pinned(&TOY, fixtures.path(), &sample());
        every_fixture_loads::<Toy, _>(&TOY, fixtures.path());
        newer_is_refused::<Toy, _>(&TOY, "toy.toml", "name = 7\n", &sample());
        unknown_keys_survive(
            &TOY,
            "toy.toml",
            &sample(),
            &["sub"],
            |t: &mut Toy| t.finished = None,
            &["finished"],
        );
    }

    /// A frozen_read format missing a down-step fails test 5.
    #[test]
    #[should_panic(expected = "frozen binaries")]
    fn frozen_read_without_down_steps_fails() {
        static BAD: Format<Toml> = Format {
            name: "toy record",
            key: "schema",
            current: 2,
            upgrades: &[toy_up_1],
            downgrades: &[],
            down_floor: 1,
            frozen_read: true,
            legacy: None,
        };
        descriptor_is_consistent(&BAD);
    }

    fn written(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    /// Sticky versions: a write keeps the file's version unless the content
    /// needs the newer one; a new file gets the oldest version that holds it.
    #[test]
    fn writes_pick_the_oldest_version_that_holds_the_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("toy.toml");
        let plain = Toy { name: "a".into(), ..Toy::default() };

        write(&path, &plain, &TOY).unwrap();
        assert_eq!(written(&path), "schema = 1\ntitle = \"a\"\n\n[sub]\nn = 0\n");
        assert_eq!(read::<Toy, _>(&path, &TOY).unwrap().unwrap(), plain);

        let colored = Toy { color: Some("red".into()), ..Toy { name: "a".into(), ..Toy::default() } };
        write(&path, &colored, &TOY).unwrap();
        assert!(
            written(&path).starts_with("schema = 2\nname = \"a\"\ncolor = \"red\"\n"),
            "{}",
            written(&path)
        );

        // Once at v2, a file stays there even when the content would fit v1.
        write(&path, &plain, &TOY).unwrap();
        assert!(written(&path).starts_with("schema = 2\nname = \"a\"\n"), "{}", written(&path));

        // A v1 file stays v1, and its unknown keys survive the down-step.
        std::fs::write(&path, "schema = 1\ntitle = \"a\"\nextra = 1\n").unwrap();
        let toy: Toy = read(&path, &TOY).unwrap().unwrap();
        write(&path, &toy, &TOY).unwrap();
        assert_eq!(written(&path), "schema = 1\nextra = 1\ntitle = \"a\"\n\n[sub]\nn = 0\n");

        // An unusable file is overwritten as if new.
        std::fs::write(&path, "not toml [").unwrap();
        write(&path, &plain, &TOY).unwrap();
        assert!(written(&path).starts_with("schema = 1\n"), "{}", written(&path));
    }

    #[test]
    fn a_version_that_is_not_a_positive_integer_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("toy.toml");
        for bad in ["schema = 0", "schema = -1", "schema = \"2\"", "schema = 1.0"] {
            std::fs::write(&path, format!("{bad}\nname = \"a\"\n")).unwrap();
            let err = format!("{:#}", read::<Toy, _>(&path, &TOY).unwrap_err());
            assert!(
                err.contains("is not a positive integer") && err.contains("toy.toml"),
                "{bad}: {err}"
            );
        }
    }

    /// A raw document that does not parse is `Unparsable` (the one failure
    /// a lock-free reader retries); a newer one and a typed failure are not.
    #[test]
    fn only_raw_parse_failures_are_unparsable() {
        let path = Path::new("/x/toy.toml");
        assert!(is_unparsable(&parse::<Toy, _>("name = ", path, &TOY).unwrap_err()));
        assert!(!is_unparsable(&parse::<Toy, _>("schema = 3\nname = 1\n", path, &TOY).unwrap_err()));
        assert!(!is_unparsable(&parse::<Toy, _>("name = 1\n", path, &TOY).unwrap_err()));
    }

    /// Put-back recurses into maps only. A key inside an array element is
    /// dropped (it could land on the wrong element); unknown keys keep their
    /// place in the file; an unknown map the writer also lacks is copied whole.
    #[test]
    fn put_back_is_maps_only_and_keeps_the_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("toy.toml");
        std::fs::write(
            &path,
            "schema = 2\nname = \"a\"\nbefore-sub = 1\n\n[sub]\nn = 1\nextra = \"s\"\n\n\
             [future]\nx = 1\n\n[[items]]\nn = 2\nlost = true\n",
        )
        .unwrap();
        let mut toy: Toy = read(&path, &TOY).unwrap().unwrap();
        toy.sub.n = 5;
        write(&path, &toy, &TOY).unwrap();
        let doc: Toml = toml::from_str(&written(&path)).unwrap();
        let keys: Vec<_> = doc.keys().map(String::as_str).collect();
        assert_eq!(keys, ["schema", "name", "before-sub", "sub", "future", "items"]);
        assert_eq!(doc["sub"]["n"].as_integer(), Some(5));
        assert_eq!(doc["sub"]["extra"].as_str(), Some("s"));
        assert_eq!(doc["future"]["x"].as_integer(), Some(1));
        assert!(doc["items"][0].get("lost").is_none(), "{doc:?}");
    }

    /// The JSON document keeps its key order (serde_json `preserve_order`).
    #[test]
    fn json_documents_keep_their_key_order() {
        let doc = <Json as Document>::parse(r#"{"z": 1, "a": 2, "m": {"y": 1, "b": 2}}"#).unwrap();
        assert_eq!(doc.render().unwrap().replace([' ', '\n'], ""), r#"{"z":1,"a":2,"m":{"y":1,"b":2}}"#);
    }
}
