//! Bringing a user-MDB overlay forward to this binary's MDB generation
//! (design/mdb-overlay-migration-plan.md).
//!
//! A login command that loads an overlay older than the binary rewrites it
//! on disk, once: under a `LinkLock`, after a full copy into
//! `<CACTUP_HOME>/machine-backups/<name>/generation-<N>/`, verified by
//! today's parse and validation before anything is written, `meta.toml`
//! last (its stamp is the commit point). Every later load, by any process,
//! finds a current overlay and says nothing.
//!
//! A step preserves behavior or refuses: it applies mechanical edits, notes
//! a difference that can only surface as a loud error, and stops where the
//! machine could quietly act differently. It never applies an optional
//! recommendation, never edits a script, and never writes a key the target
//! generation's first binary lacks. `STEPS[i]` migrates generation i+1 to
//! i+2; every generation bump adds one (`every_generation_has_a_migration`).
//!
//! An overlay newer than the binary is refused, never downgraded. One with
//! no recorded generation is assumed current and never rewritten. Nothing
//! here runs on the compute path (D11): `main` never opens the MDB there,
//! and `COMPUTE_NODE` is the backstop.

use super::{optionlist, Layer, Machine, Meta};
use crate::lock::LinkLock;
use crate::Res;
use anyhow::{bail, Context};
use colored::Colorize;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use toml_edit::DocumentMut;

/// Set by `main` on the compute-node side of a job (D11). Such a process
/// never opens the MDB, so this is a backstop: should one ever load an
/// overlay, it is refused as before instead of rewritten.
pub static COMPUTE_NODE: AtomicBool = AtomicBool::new(false);

/// How long a load waits for another process that is migrating the same
/// overlay. The work itself takes milliseconds; a holder this slow is stuck.
const LOCK_WAIT: Duration = Duration::from_secs(60);

/// The documentation fallback when the system MDB ships no
/// `GENERATIONS.md`: the copy published on the `mdb` branch, whose
/// `## Generation N` headings the host renders with `#generation-N`
/// anchors. (The documentation site's own generations page has no
/// per-generation sections to point at.)
pub const GENERATIONS_URL: &str = "https://github.com/max-morris/Cactup/blob/mdb/GENERATIONS.md";

// ---------------------------------------------------------------------------
// Steps

/// One generation's migration.
pub struct Step {
    /// Edit the overlay in memory: the notes to print, or why it must stop.
    pub run: fn(&mut Overlay) -> Result<Vec<String>, Stop>,
    /// Whether the target generation's `GENERATIONS.md` entry lists
    /// optional changes, which a step never makes; the notice points at
    /// them.
    pub optional: bool,
}

/// `STEPS[i]` migrates generation i+1 to i+2. The framework stamps the
/// generation reached, so a step that changes nothing else is `raise`.
pub static STEPS: &[Step] = &[
    // 1 → 2: never automatic (Max's decision 4): the hand recipe.
    Step { run: refuse_generation_1, optional: false },
    // 2 → 3: placeholder until the key renames land (plan phase P2).
    Step { run: not_implemented_yet, optional: true },
    // 3 → 4: `inputs-home` is not added; without it inputs go to
    // `~/.cactup/inputs`, the generation-4 default.
    Step { run: raise, optional: true },
    // 4 → 5: scripts untouched: `@ALLOCATION@`/`ALLOCATION` still work, and
    // their new meaning comes from the binary, whatever the overlay says.
    Step { run: raise, optional: true },
];

/// Why the transaction did not finish: a step (or a file the steps need)
/// stopped it, or a condition the user clears and then simply retries.
enum Failure {
    Stop(Stop),
    Transient(String),
}

impl From<Stop> for Failure {
    fn from(stop: Stop) -> Failure {
        Failure::Stop(stop)
    }
}

/// Why a step stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// This generation is never brought forward automatically.
    Unsupported,
    /// What the step found that it cannot carry forward unchanged: one
    /// clause, no trailing period ("its build-submit script … reads …").
    Found(String),
}

fn raise(_: &mut Overlay) -> Result<Vec<String>, Stop> {
    Ok(Vec::new())
}

fn refuse_generation_1(_: &mut Overlay) -> Result<Vec<String>, Stop> {
    Err(Stop::Unsupported)
}

fn not_implemented_yet(_: &mut Overlay) -> Result<Vec<String>, Stop> {
    Err(Stop::Found("updating from generation 2 to 3 is not implemented yet".to_owned()))
}

// ---------------------------------------------------------------------------
// The overlay in memory

/// A TOML file of the overlay, as read and as edited.
pub struct TomlFile {
    /// Its path relative to the overlay directory.
    pub rel: String,
    original: String,
    pub doc: DocumentMut,
}

impl TomlFile {
    fn parse(rel: String, original: String) -> Result<TomlFile, Stop> {
        let doc = original
            .parse::<DocumentMut>()
            .map_err(|e| Stop::Found(format!("{rel} is not valid TOML ({})", first_line(&e.to_string()))))?;
        Ok(TomlFile { rel, original, doc })
    }

    fn changed(&self) -> Option<String> {
        let text = self.doc.to_string();
        (text != self.original).then_some(text)
    }
}

/// What the steps work on: `meta.toml` and every optionlist as editable
/// documents (comments kept), and the script files, for checks only —
/// scripts are never edited.
pub struct Overlay {
    pub name: String,
    pub dir: PathBuf,
    pub meta: TomlFile,
    /// `optionlists/*.toml`, the files `Machine::flavors` lists.
    pub optionlists: Vec<TomlFile>,
    /// Script files relative to the overlay (`buildsubmitscripts/build.py`).
    #[allow(dead_code)] // read by the 2→3 checks (plan phase P2)
    pub scripts: Vec<String>,
}

impl Overlay {
    /// The overlay at `dir`, or why not: a file that is not TOML stops the
    /// migration; one that cannot be read is a condition to clear first.
    fn read(name: &str, dir: &Path, meta_text: String) -> Result<Overlay, Failure> {
        let unreadable = |path: &Path, e: std::io::Error| {
            Failure::Transient(format!("'{}' could not be read ({e})", path.display()))
        };
        let meta = TomlFile::parse("meta.toml".to_owned(), meta_text)?;
        let mut optionlists = Vec::new();
        for stem in list_optionlists(dir).map_err(|e| unreadable(&dir.join("optionlists"), e))? {
            let path = dir.join("optionlists").join(format!("{stem}.toml"));
            let text = fs::read_to_string(&path).map_err(|e| unreadable(&path, e))?;
            optionlists.push(TomlFile::parse(format!("optionlists/{stem}.toml"), text)?);
        }
        let mut scripts = Vec::new();
        for kind in ["submitscripts", "runscripts", "buildsubmitscripts"] {
            let entries = match fs::read_dir(dir.join(kind)) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                other => other.map_err(|e| unreadable(&dir.join(kind), e))?,
            };
            for entry in entries.flatten() {
                if entry.path().is_file() {
                    scripts.push(format!("{kind}/{}", entry.file_name().to_string_lossy()));
                }
            }
        }
        scripts.sort();
        Ok(Overlay { name: name.to_owned(), dir: dir.to_owned(), meta, optionlists, scripts })
    }

    /// Stamp `[cactup] mdb-generation`, keeping the line's layout and
    /// comments. The gate only gets here when the key holds an integer, in
    /// whatever form `[cactup]` takes (a table, inline, or dotted keys).
    fn stamp(&mut self, generation: u32) -> Res<()> {
        let value = self
            .meta
            .doc
            .get_mut("cactup")
            .and_then(|cactup| cactup.get_mut("mdb-generation"))
            .and_then(|item| item.as_value_mut())
            .context("meta.toml has no [cactup] mdb-generation to update")?;
        let decor = value.decor().clone();
        *value = toml_edit::Value::from(i64::from(generation));
        *value.decor_mut() = decor;
        Ok(())
    }

    /// Today's parse and validation, on the text about to be written: the
    /// same checks `Machine::load` makes, and every flavor's header.
    fn verify(&self) -> Res<()> {
        let meta: Meta = toml::from_str(&self.meta.doc.to_string()).context("Failed to parse meta.toml")?;
        meta.validate(&self.name)?;
        let machine = Machine { name: self.name.clone(), dir: self.dir.clone(), layer: Layer::User, meta };
        machine.validate_files()?;
        for optionlist in &self.optionlists {
            optionlist::parse_header(&optionlist.doc.to_string())
                .with_context(|| format!("Invalid optionlist '{}'", optionlist.rel))?;
        }
        Ok(())
    }
}

/// The flavor stems in `<dir>/optionlists`, as `Machine::flavors` lists them.
fn list_optionlists(dir: &Path) -> std::io::Result<Vec<String>> {
    let entries = match fs::read_dir(dir.join("optionlists")) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        other => other?,
    };
    let mut stems = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "toml")
            && path.is_file()
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            && !stem.starts_with('.')
        {
            stems.push(stem.to_owned());
        }
    }
    stems.sort();
    Ok(stems)
}

// ---------------------------------------------------------------------------
// Reading the recorded generation

/// What a raw `meta.toml` says about its generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    /// Not TOML, or a value that is not a generation: the typed parse that
    /// follows reports it properly.
    Unreadable,
    /// No `[cactup] mdb-generation`: assumed current, never rewritten.
    Missing,
    At(u32),
}

pub fn recorded(text: &str) -> Recorded {
    let Ok(table) = text.parse::<toml::Table>() else { return Recorded::Unreadable };
    match table.get("cactup").and_then(|c| c.get("mdb-generation")) {
        Some(toml::Value::Integer(n)) => u32::try_from(*n).map_or(Recorded::Unreadable, Recorded::At),
        Some(_) => Recorded::Unreadable,
        None => Recorded::Missing,
    }
}

/// The older generation an unmarked overlay's keys belong to, when they
/// give one away: a `variants` table was generation 1, the `env-*-setup`
/// keys generation 2.
pub fn looks_like(text: &str) -> Option<u32> {
    let table = text.parse::<toml::Table>().ok()?;
    if table.contains_key("variants") {
        return Some(1);
    }
    let old_keys = |t: &toml::Table| {
        ["env-build-setup", "env-submit-setup", "env-run-setup"].iter().any(|k| t.contains_key(*k))
    };
    let environment = table.get("environment").and_then(toml::Value::as_table);
    let universes = table.get("universes").and_then(toml::Value::as_table);
    let in_universe = universes.is_some_and(|u| u.values().filter_map(toml::Value::as_table).any(old_keys));
    (environment.is_some_and(old_keys) || in_universe).then_some(2)
}

/// The `--from-existing` base a raw `meta.toml` records, for "start over".
fn origin_from(text: &str) -> Option<String> {
    let table = text.parse::<toml::Table>().ok()?;
    let from = table.get("cactup")?.get("origin")?.get("from")?.as_str()?;
    Some(from.to_owned())
}

// ---------------------------------------------------------------------------
// Refusals

/// A user-MDB overlay of another generation that cannot be used as it is:
/// older, and it could not be brought forward, or newer than this binary.
#[derive(Debug)]
pub struct OverlayGenerationError {
    /// The machine name.
    pub name: String,
    /// The overlay's `meta.toml`: the file to edit.
    pub path: PathBuf,
    /// The generation the overlay records now.
    pub found: u32,
    /// This binary's generation.
    pub want: u32,
    /// Where the generation changes are described: a local `GENERATIONS.md`
    /// or its published copy.
    pub guide: String,
    /// `<CACTUP_HOME>/machine-backups`, where "start over" moves the old
    /// files.
    pub backups: PathBuf,
    /// The `machine create` command that starts this machine over, when its
    /// base still exists (`None`: there is nothing to start over from).
    pub recreate: Option<String>,
    pub why: Why,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Why {
    /// Newer than this binary. `kept` is the copy of the overlay as it was
    /// for this generation, when a newer cactup kept one.
    Newer { kept: Option<PathBuf> },
    /// Older, and this generation is never updated automatically.
    Unsupported,
    /// Older, and updating stopped on `finding`. `from` is where it started
    /// when earlier steps were written this time (so `found` is where they
    /// got to).
    Stopped { finding: String, from: Option<u32> },
    /// Older, and updating could not proceed just now (the lock, a read or
    /// a write failed): nothing to fix by hand, retry once `reason` is
    /// cleared. Whatever was written is finished by the next load.
    Transient { reason: String },
}

impl OverlayGenerationError {
    /// The short form for a one-line listing (`machine list`).
    pub fn brief(&self) -> String {
        let Self { name, found, .. } = self;
        match &self.why {
            Why::Newer { .. } => format!("needs MDB generation {found}; run `cactup update`"),
            Why::Unsupported | Why::Stopped { from: None, .. } => format!(
                "MDB generation {found}, could not be updated automatically; see `cactup machine show {name}`"
            ),
            Why::Transient { .. } => {
                format!("MDB generation {found}, could not be updated just now; see `cactup machine show {name}`")
            }
            Why::Stopped { from: Some(from), .. } => format!(
                "MDB generation {found} (updated from {from}), could not be updated further; \
                 see `cactup machine show {name}`"
            ),
        }
    }
}

impl std::fmt::Display for OverlayGenerationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { name, path, found, want, guide, backups, recreate, why } = self;
        match why {
            Why::Newer { kept } => {
                write!(f, "user-MDB machine \"{name}\" is MDB generation {found}, and this cactup reads {want}: run `cactup update`")?;
                if let Some(kept) = kept {
                    write!(
                        f,
                        "\n  A copy as it was for generation {want}, from before a newer cactup brought it \
                         forward, is in '{}'.",
                        kept.display()
                    )?;
                }
                return Ok(());
            }
            Why::Unsupported => write!(
                f,
                "user-MDB machine \"{name}\" is MDB generation {found}, which cactup does not update \
                 automatically (this cactup reads {want})."
            )?,
            Why::Stopped { finding, from: None } => write!(
                f,
                "user-MDB machine \"{name}\" is MDB generation {found}, and this cactup reads {want}. \
                 It could not be updated automatically: {finding}."
            )?,
            Why::Stopped { finding, from: Some(from) } => write!(
                f,
                "user-MDB machine \"{name}\" was updated from MDB generation {from} to {found}, and this \
                 cactup reads {want}. It could not be updated further: {finding}."
            )?,
            Why::Transient { reason } => {
                return write!(
                    f,
                    "user-MDB machine \"{name}\" is MDB generation {found}, and this cactup reads {want}. \
                     It could not be updated just now: {reason}.\n  \
                     Nothing needs editing by hand: once that is cleared, run the command again and cactup \
                     finishes the update."
                );
            }
        }
        // Generation 0 is no generation; the first recipe is generation 2's.
        let next = (*found).max(1) + 1;
        write!(
            f,
            "\n  To fix by hand: follow {}, then set `mdb-generation = {next}` in '{}'; cactup does the rest.",
            guide_section(guide, next),
            path.display()
        )?;
        // Starting over moves the old files aside rather than deleting
        // them: a refusal before the first step takes no backup.
        let Some(recreate) = recreate else { return Ok(()) };
        let aside = backups.join(name);
        let dir = path.parent().unwrap_or(path);
        write!(
            f,
            "\n  Or start over (a fresh copy without your edits; the old files are moved aside): \
             mkdir -p '{}' && mv '{}' '{}' && {recreate}",
            aside.display(),
            dir.display(),
            aside.join("before-start-over").display()
        )
    }
}

impl std::error::Error for OverlayGenerationError {}

fn is_url(guide: &str) -> bool {
    guide.starts_with("https://") || guide.starts_with("http://")
}

/// Where to read one generation's entry: its anchor in the published copy,
/// or its heading in a local `GENERATIONS.md`.
pub fn guide_section(guide: &str, generation: u32) -> String {
    if is_url(guide) {
        format!("{guide}#generation-{generation}")
    } else {
        format!("\"Generation {generation}\" in '{guide}'")
    }
}

fn guide_ref(guide: &str) -> String {
    if is_url(guide) { guide.to_owned() } else { format!("'{guide}'") }
}

/// The newest copy of `name` as it was for generation `generation`, if a
/// migration from it kept one.
pub fn kept_copy(backups: &Path, name: &str, generation: u32) -> Option<PathBuf> {
    let mut newest = None;
    for k in 1.. {
        let dir = backups.join(name).join(backup_name(generation, k));
        if fs::symlink_metadata(&dir).is_err() {
            return newest;
        }
        newest = Some(dir);
    }
    unreachable!()
}

fn backup_name(generation: u32, k: u32) -> String {
    if k == 1 { format!("generation-{generation}") } else { format!("generation-{generation}.{k}") }
}

// ---------------------------------------------------------------------------
// The transaction

/// Where an overlay lives and what to tell its user.
pub struct Place<'a> {
    pub name: &'a str,
    /// The overlay directory (`<user root>/<name>`).
    pub dir: &'a Path,
    /// `<CACTUP_HOME>/machine-backups`.
    pub backups: &'a Path,
    /// The system MDB, where a `--from-existing` base may live.
    pub system_root: &'a Path,
    pub guide: &'a str,
}

impl Place<'_> {
    fn refusal(&self, found: u32, want: u32, origin: Option<String>, why: Why) -> OverlayGenerationError {
        OverlayGenerationError {
            name: self.name.to_owned(),
            path: self.dir.join("meta.toml"),
            found,
            want,
            guide: self.guide.to_owned(),
            backups: self.backups.to_owned(),
            recreate: self.recreate(origin),
            why,
        }
    }

    /// The command that re-creates this machine from the base it records,
    /// once it has been moved aside: `None` when that base would not resolve
    /// any more (retired from the system MDB, or another overlay since
    /// deleted). An overlay without an origin came from `generic`, which
    /// always exists.
    fn recreate(&self, origin: Option<String>) -> Option<String> {
        let name = self.name;
        let Some(base) = origin else { return Some(format!("cactup machine create {name}")) };
        let user_root = self.dir.parent().unwrap_or(self.dir);
        let exists = base == "generic"
            || self.system_root.join(&base).join("meta.toml").is_file()
            || (base != name && user_root.join(&base).join("meta.toml").is_file());
        exists.then(|| format!("cactup machine create {name} --from-existing {base}"))
    }

    /// The refusal of an overlay newer than this binary.
    pub fn newer(&self, found: u32, origin: Option<String>) -> OverlayGenerationError {
        let want = crate::build_info::MDB_GENERATION;
        let kept = kept_copy(self.backups, self.name, want);
        self.refusal(found, want, origin, Why::Newer { kept })
    }
}

/// Bring the overlay at `place`, which an unlocked read found at generation
/// `found` (older than this binary's), forward to this binary's generation.
/// `Ok` when it is current afterward, by this process or another; an
/// `OverlayGenerationError` when it could not be.
pub fn bring_forward(place: &Place, found: u32) -> Res<()> {
    bring_forward_with(place, found, STEPS, crate::build_info::MDB_GENERATION)
}

fn bring_forward_with(place: &Place, found: u32, steps: &[Step], want: u32) -> Res<()> {
    let meta_path = place.dir.join("meta.toml");
    let stopped = |found: u32, origin: Option<String>, finding: String| {
        place.refusal(found, want, origin, Why::Stopped { finding, from: None })
    };
    if on_compute_node() {
        let finding = format!("a job never updates machines; run `cactup machine show {}` first", place.name);
        let origin = fs::read_to_string(&meta_path).ok().and_then(|text| origin_from(&text));
        return Err(stopped(found, origin, finding).into());
    }

    // 1. One migrator per overlay, across processes and hosts. The lock
    // lives beside the overlay, never inside it (`machines()` lists only
    // directories with a meta.toml).
    // Anything that stops it short of a step's verdict (the lock, a read, a
    // write) is transient: say what, and that a retry finishes the job.
    let unlocked_origin = fs::read_to_string(&meta_path).ok().and_then(|text| origin_from(&text));
    let transient = |found: u32, origin: Option<String>, e: anyhow::Error| -> anyhow::Error {
        if gix::interrupt::is_triggered() {
            return e;
        }
        place.refusal(found, want, origin, Why::Transient { reason: format!("{e:#}") }).into()
    };
    // An overlay whose first step refuses needs no lock and gets nothing
    // written: refuse it from an unlocked read, so a lock that cannot be
    // taken never hides the real verdict.
    if let Ok(text) = fs::read_to_string(&meta_path)
        && let Recorded::At(n) = recorded(&text)
        && n < want
    {
        plan(place, n, text, unlocked_origin.clone(), steps, want)?;
    }
    let lock_path = place.dir.with_file_name(format!(".{}.migrate.lock", place.name));
    let acquired = LinkLock::try_acquire(&lock_path);
    let _lock = match acquired {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            eprintln!("Waiting for another cactup updating user-MDB machine \"{}\"…", place.name);
            LinkLock::acquire_wait(&lock_path, lock_wait())
                .map_err(|e| transient(found, unlocked_origin.clone(), e))?
        }
        Err(e) => return Err(transient(found, unlocked_origin, e)),
    };

    // 2. Re-read under the lock: another process may have finished first.
    let text = fs::read_to_string(&meta_path)
        .with_context(|| format!("Failed to read '{}'", meta_path.display()))
        .map_err(|e| transient(found, unlocked_origin, e))?;
    let origin = origin_from(&text);
    let found = match recorded(&text) {
        Recorded::At(n) if n > want => return Err(place.newer(n, origin).into()),
        Recorded::At(n) if n < want => n,
        _ => return Ok(()),
    };

    // 3. Every step, in memory. Nothing has been written yet, and nothing
    // is until the result is known to be worth writing.
    let Planned { mut overlay, reached, notes, stop } = plan(place, found, text, origin.clone(), steps, want)?;
    // 4. Only a complete chain can be checked against today's schema; a
    // partial one stops at an older generation, which the user finishes by
    // hand from the recipe the refusal names.
    if stop.is_none()
        && let Err(e) = overlay.verify()
    {
        let finding = format!("the updated machine would not load ({e:#})");
        return Err(stopped(found, origin, finding).into());
    }
    overlay.stamp(reached)?;

    // 5. Back up, then commit, `meta.toml` last.
    let backup = back_up(place, found).map_err(|e| transient(found, origin.clone(), e))?;
    if gix::interrupt::is_triggered() {
        bail!("Interrupted");
    }
    let committing = backup.join(COMMITTING);
    write_atomic(&committing, "").map_err(|e| transient(found, origin.clone(), e))?;
    commit(&overlay).map_err(|e| {
        let e = e.context(format!("the old files are in '{}'", backup.display()));
        transient(found, origin.clone(), e)
    })?;
    // Committed: the overlay is at `reached` whatever happens to the notes.
    if let Err(e) = finish_backup(&backup, place.name, found, reached, &notes, stop.as_ref()) {
        eprintln!(
            "{}",
            format!("Warning: could not finish the record of the update in '{}': {e:#}", backup.display()).yellow()
        );
    }

    // 6. Say so, once.
    let optional: Vec<String> = (found + 1..=reached)
        .filter(|&g| steps.get(g as usize - 2).is_some_and(|s| s.optional))
        .map(|g| g.to_string())
        .collect();
    eprintln!(
        "{}",
        format!(
            "Updated user-MDB machine \"{}\" from MDB generation {found} to {reached}; the old files are in '{}'.",
            place.name,
            backup.display()
        )
        .bright_green()
    );
    for note in &notes {
        eprintln!("{}", format!("  note: {note}").yellow());
    }
    if stop.is_none() && !optional.is_empty() {
        let plural = if optional.len() == 1 { "generation" } else { "generations" };
        eprintln!("  Optional changes for {plural} {} are described in {}.", optional.join(", "), guide_ref(place.guide));
    }
    match stop {
        None => Ok(()),
        Some(stop) => Err(refuse(place, reached, want, origin, stop, Some(found)).into()),
    }
}

/// The steps' result, in memory: the edited overlay, the generation it
/// reached, the notes, and what stopped it short of this generation.
struct Planned {
    overlay: Overlay,
    reached: u32,
    notes: Vec<String>,
    stop: Option<Stop>,
}

/// Run the steps from `found` over the overlay whose `meta.toml` reads
/// `text`. A refusal when there is nothing to write: the first step
/// stopped, or the files could not be read.
fn plan(place: &Place, found: u32, text: String, origin: Option<String>, steps: &[Step], want: u32) -> Res<Planned> {
    let mut overlay = match Overlay::read(place.name, place.dir, text) {
        Ok(overlay) => overlay,
        Err(Failure::Stop(stop)) => return Err(refuse(place, found, want, origin, stop, None).into()),
        Err(Failure::Transient(reason)) => {
            return Err(place.refusal(found, want, origin, Why::Transient { reason }).into());
        }
    };
    let mut reached = found;
    let mut notes = Vec::new();
    let mut stop = None;
    for generation in found.max(1)..want {
        let Some(step) = steps.get(generation as usize - 1) else {
            stop = Some(Stop::Found(format!("this cactup has no update from generation {generation}")));
            break;
        };
        match (step.run)(&mut overlay) {
            Ok(mut more) => {
                notes.append(&mut more);
                reached = generation + 1;
            }
            Err(why) => {
                stop = Some(why);
                break;
            }
        }
    }
    if reached == found {
        // The first step stopped: nothing to write, not even a backup.
        let stop = stop.expect("a step ran or stopped");
        return Err(refuse(place, found, want, origin, stop, None).into());
    }
    Ok(Planned { overlay, reached, notes, stop })
}

fn refuse(place: &Place, found: u32, want: u32, origin: Option<String>, stop: Stop, from: Option<u32>) -> OverlayGenerationError {
    let why = match stop {
        Stop::Unsupported => Why::Unsupported,
        Stop::Found(finding) => Why::Stopped { finding, from },
    };
    place.refusal(found, want, origin, why)
}

fn lock_wait() -> Duration {
    #[cfg(test)]
    if let Some(wait) = tests::LOCK_WAIT_HERE.with(|w| w.get()) {
        return wait;
    }
    LOCK_WAIT
}

fn on_compute_node() -> bool {
    #[cfg(test)]
    if tests::COMPUTE_NODE_HERE.with(|c| c.get()) {
        return true;
    }
    COMPUTE_NODE.load(Ordering::Relaxed)
}

/// In a backup, written just before the commit starts: the overlay may
/// hold edited files from here on, so this copy is the only one from
/// before them.
const COMMITTING: &str = ".committing";

/// The backup directory for a migration from `found`: a fresh full copy at
/// `generation-<found>` (or `.2`, `.3`, … past ones a finished migration
/// kept), or the copy a run of this same migration left when it was
/// interrupted mid-commit (`.committing`, no `.complete`), which holds the
/// files from before it touched any. A copy is published by renaming a
/// finished one into place, so a directory under its final name is always
/// whole. One with neither marker never reached the commit, so the overlay
/// may have changed since (the user edited it, or deleted and re-created
/// the machine): it is replaced by a fresh copy.
fn back_up(place: &Place, found: u32) -> Res<PathBuf> {
    let parent = place.backups.join(place.name);
    fs::create_dir_all(&parent).with_context(|| format!("Failed to create '{}'", parent.display()))?;
    for k in 1.. {
        let dir = parent.join(backup_name(found, k));
        if fs::symlink_metadata(&dir).is_ok() {
            if dir.join(".complete").exists() {
                continue;
            }
            if dir.join(COMMITTING).exists() {
                return Ok(dir);
            }
            fs::remove_dir_all(&dir).with_context(|| format!("Failed to replace the stale copy '{}'", dir.display()))?;
        }
        let temp = tempfile::Builder::new()
            .prefix(".copy-")
            .tempdir_in(&parent)
            .with_context(|| format!("Failed to create a temporary directory in '{}'", parent.display()))?;
        copy_tree(place.dir, temp.path())?;
        fs::rename(temp.path(), &dir)
            .with_context(|| format!("Failed to move the backup into place at '{}'", dir.display()))?;
        let _ = temp.keep(); // renamed away: nothing left for it to remove
        return Ok(dir);
    }
    unreachable!()
}

/// A full copy of `from` into the existing directory `to`, minus Python's
/// caches; links stay links.
fn copy_tree(from: &Path, to: &Path) -> Res<()> {
    for entry in fs::read_dir(from).with_context(|| format!("Failed to list '{}'", from.display()))? {
        if gix::interrupt::is_triggered() {
            bail!("Interrupted");
        }
        let entry = entry.with_context(|| format!("Failed to list '{}'", from.display()))?;
        let name = entry.file_name();
        if name == "__pycache__" {
            continue;
        }
        let (source, target) = (entry.path(), to.join(&name));
        let kind = entry.file_type().with_context(|| format!("Failed to inspect '{}'", source.display()))?;
        if kind.is_dir() {
            fs::create_dir(&target).with_context(|| format!("Failed to create '{}'", target.display()))?;
            copy_tree(&source, &target)?;
        } else if kind.is_symlink() {
            let link = fs::read_link(&source).with_context(|| format!("Failed to read '{}'", source.display()))?;
            std::os::unix::fs::symlink(&link, &target)
                .with_context(|| format!("Failed to create '{}'", target.display()))?;
        } else {
            fs::copy(&source, &target).with_context(|| format!("Failed to copy '{}'", source.display()))?;
        }
    }
    Ok(())
}

/// Write every changed file, `meta.toml` last: until its stamp lands, a
/// reader still sees the old generation and comes for the lock, and the
/// next load redoes the whole chain, every edit of which is idempotent.
fn commit(overlay: &Overlay) -> Res<()> {
    let mut written = 0;
    for file in &overlay.optionlists {
        if let Some(text) = file.changed() {
            write_atomic(&overlay.dir.join(&file.rel), &text)?;
            written += 1;
            fault_point(written)?;
        }
    }
    let text = overlay.meta.doc.to_string();
    write_atomic(&overlay.dir.join(&overlay.meta.rel), &text)
}

/// Record what was done next to the copy, then mark the migration finished.
fn finish_backup(dir: &Path, name: &str, found: u32, reached: u32, notes: &[String], stop: Option<&Stop>) -> Res<()> {
    let mut text = format!(
        "cactup {} updated user-MDB machine \"{name}\" from MDB generation {found} to {reached}.\n\
         This directory holds its files as they were before; to go back, copy them over the machine's directory.\n",
        crate::VERSION
    );
    if !notes.is_empty() {
        text.push_str("\nNotes:\n");
        for note in notes {
            text.push_str(&format!("  - {note}\n"));
        }
    }
    match stop {
        None => {}
        Some(Stop::Unsupported) => {
            text.push_str(&format!("\nStopped at generation {reached}, which is not updated automatically.\n"))
        }
        Some(Stop::Found(finding)) => text.push_str(&format!("\nStopped at generation {reached}: {finding}.\n")),
    }
    #[cfg(test)]
    if tests::FAIL_FINISH.with(|f| f.get()) {
        bail!("injected fault in the record");
    }
    write_atomic(&dir.join("NOTES.txt"), &text)?;
    write_atomic(&dir.join(".complete"), "")
}

#[cfg(test)]
fn fault_point(written: usize) -> Res<()> {
    if tests::FAULT_AFTER.with(|f| f.get()) == Some(written) {
        bail!("injected fault after {written} file(s)");
    }
    Ok(())
}

#[cfg(not(test))]
fn fault_point(_: usize) -> Res<()> {
    Ok(())
}

/// Atomic write (temp file in the same directory + fsync + rename), so a
/// reader sees the old file or the new one, never a torn one. The file
/// keeps the mode it had, and a new one gets the mode a plain write would
/// give it (0666 less the umask), not the temp file's owner-only 0600.
// A copy of format::write_atomic on the unmerged format-versioning branch;
// fold onto it when that lands.
fn write_atomic(path: &Path, text: &str) -> Res<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = path.parent().expect("overlay paths have parents");
    let mut temp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("Failed to create a temp file in '{}'", dir.display()))?;
    let mode = match fs::metadata(path) {
        Ok(meta) => Some(meta.permissions().mode() & 0o7777),
        Err(_) => umask().map(|mask| 0o666 & !mask),
    };
    if let Some(mode) = mode {
        temp.as_file()
            .set_permissions(fs::Permissions::from_mode(mode))
            .with_context(|| format!("Failed to write '{}'", path.display()))?;
    }
    temp.write_all(text.as_bytes())
        .and_then(|()| temp.as_file().sync_all())
        .with_context(|| format!("Failed to write '{}'", path.display()))?;
    temp.persist(path)
        .with_context(|| format!("Failed to move '{}' into place", path.display()))?;
    Ok(())
}

/// This process's umask, from `/proc` (std has no call for it, and the
/// binary takes no C library binding: D13). `None` where `/proc` does not
/// say; the file then stays owner-only.
fn umask() -> Option<u32> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let value = status.lines().find_map(|line| line.strip_prefix("Umask:"))?;
    u32::from_str_radix(value.trim(), 8).ok()
}

fn first_line(text: &str) -> &str {
    text.lines().find(|l| !l.trim().is_empty()).unwrap_or(text).trim()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::build_info::MDB_GENERATION;
    use crate::commands::machine::hash_machine_dir;
    use crate::mdb::Mdb;
    use std::cell::Cell;

    thread_local! {
        /// Fail the commit after this many optionlist writes.
        pub(super) static FAULT_AFTER: Cell<Option<usize>> = const { Cell::new(None) };
        /// Behave as a compute-node run, on this thread only (the global
        /// flag would leak into tests running beside it).
        pub(super) static COMPUTE_NODE_HERE: Cell<bool> = const { Cell::new(false) };
        /// Wait this long for the lock instead of `LOCK_WAIT`.
        pub(super) static LOCK_WAIT_HERE: Cell<Option<Duration>> = const { Cell::new(None) };
        /// Fail writing NOTES.txt/.complete, after the commit.
        pub(super) static FAIL_FINISH: Cell<bool> = const { Cell::new(false) };
    }

    #[test]
    fn every_generation_has_a_migration() {
        assert_eq!(
            STEPS.len(),
            MDB_GENERATION as usize - 1,
            "src/mdb/migrate.rs: STEPS needs one step per older MDB generation; a bump to generation \
             {MDB_GENERATION} adds the step {} → {MDB_GENERATION} (one that only raises the number is \
             still a step)",
            MDB_GENERATION - 1
        );
    }

    /// A scratch home: `<tmp>/machines` is the user layer, so backups land
    /// in `<tmp>/machine-backups`, inside the scratch directory.
    pub(crate) struct Home {
        _tmp: tempfile::TempDir,
        pub mdb: Mdb,
    }

    impl Home {
        pub fn new() -> Home {
            let tmp = tempfile::tempdir().unwrap();
            let system = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mdb");
            let mdb = Mdb::with_roots(system, tmp.path().join("machines"));
            fs::create_dir_all(&mdb.user_root).unwrap();
            Home { _tmp: tmp, mdb }
        }

        pub fn dir(&self, name: &str) -> PathBuf {
            self.mdb.user_root.join(name)
        }

        pub fn backups(&self) -> PathBuf {
            self.mdb.backup_root()
        }

        fn place<'a>(&'a self, name: &'a str, dir: &'a Path, backups: &'a Path) -> Place<'a> {
            Place { name, dir, backups, system_root: &self.mdb.system_root, guide: "/mdb/GENERATIONS.md" }
        }

        /// A loadable overlay `name` at `generation`, with comments.
        pub fn overlay(&self, name: &str, generation: u32) -> PathBuf {
            let dir = self.dir(name);
            for sub in ["optionlists", "runscripts", "submitscripts"] {
                fs::create_dir_all(dir.join(sub)).unwrap();
            }
            let meta = format!(
                "# My machine. Hand-tuned.\n\
                 [machine]\n\
                 nickname = \"{name}\"   # what I call it\n\
                 \n\
                 [queues.local]\n\
                 default = true\n\
                 [submitscripts]\n\"default\" = [\"local\"]\n\
                 [runscripts]\n\"default\" = [\"local\"]\n\
                 \n\
                 # Written by cactup machine create.\n\
                 [cactup]\n\
                 mdb-generation = {generation}  # keep\n"
            );
            fs::write(dir.join("meta.toml"), meta).unwrap();
            fs::write(
                dir.join("optionlists/default.toml"),
                "# flavor notes\n[cactup]\ndefault = true # mine\n\n[options]\nVERSION = \"1\"\n",
            )
            .unwrap();
            fs::write(dir.join("runscripts/default.sh"), "#!/bin/sh\n").unwrap();
            fs::write(dir.join("submitscripts/default.sh"), "#!/bin/sh\n").unwrap();
            dir
        }
    }

    pub(crate) fn overlay_error(e: &anyhow::Error) -> &OverlayGenerationError {
        e.downcast_ref::<OverlayGenerationError>()
            .unwrap_or_else(|| panic!("not a generation error: {e:#}"))
    }

    fn stamp_line(text: &str) -> &str {
        text.lines().find(|l| l.starts_with("mdb-generation")).unwrap()
    }

    #[test]
    fn an_older_overlay_is_brought_forward_once_keeping_its_comments() {
        let home = Home::new();
        let dir = home.overlay("box", MDB_GENERATION - 1);
        let before = fs::read_to_string(dir.join("meta.toml")).unwrap();
        let optionlist = fs::read_to_string(dir.join("optionlists/default.toml")).unwrap();

        let machine = home.mdb.load("box").unwrap();
        assert_eq!(machine.meta.cactup.mdb_generation, Some(MDB_GENERATION));
        let after = fs::read_to_string(dir.join("meta.toml")).unwrap();
        // Only the number changed: every comment and the line's own layout stay.
        assert_eq!(
            after,
            before.replace(
                &format!("mdb-generation = {}  # keep", MDB_GENERATION - 1),
                &format!("mdb-generation = {MDB_GENERATION}  # keep")
            )
        );
        assert_eq!(stamp_line(&after), format!("mdb-generation = {MDB_GENERATION}  # keep"));
        assert_eq!(fs::read_to_string(dir.join("optionlists/default.toml")).unwrap(), optionlist);

        // The backup holds the files as they were, and says what happened.
        let backup = home.backups().join("box").join(format!("generation-{}", MDB_GENERATION - 1));
        assert_eq!(fs::read_to_string(backup.join("meta.toml")).unwrap(), before);
        assert!(backup.join("runscripts/default.sh").is_file());
        assert!(backup.join(".complete").is_file());
        let notes = fs::read_to_string(backup.join("NOTES.txt")).unwrap();
        assert!(notes.contains(&format!("from MDB generation {} to {MDB_GENERATION}", MDB_GENERATION - 1)), "{notes}");
        // The lock is gone.
        assert!(!home.mdb.user_root.join(".box.migrate.lock").exists());

        // Idempotent: a second load writes nothing, anywhere.
        let (overlay_hash, backups_hash) = (hash_machine_dir(&dir).unwrap(), hash_machine_dir(&home.backups()).unwrap());
        home.mdb.load("box").unwrap();
        assert_eq!(hash_machine_dir(&dir).unwrap(), overlay_hash);
        assert_eq!(hash_machine_dir(&home.backups()).unwrap(), backups_hash);
        assert_eq!(fs::read_dir(home.backups().join("box")).unwrap().count(), 1);
    }

    #[test]
    fn a_kept_backup_is_never_overwritten() {
        let home = Home::new();
        let dir = home.overlay("box", MDB_GENERATION - 1);
        home.mdb.load("box").unwrap();
        // The user goes back to the old files, then loads again.
        let old = home.backups().join("box").join(format!("generation-{}", MDB_GENERATION - 1));
        fs::copy(old.join("meta.toml"), dir.join("meta.toml")).unwrap();
        home.mdb.load("box").unwrap();
        let again = home.backups().join("box").join(format!("generation-{}.2", MDB_GENERATION - 1));
        assert!(again.join(".complete").is_file());
        assert!(old.join(".complete").is_file());
        // A newer overlay's refusal names the newest copy for this generation.
        assert_eq!(kept_copy(&home.backups(), "box", MDB_GENERATION - 1), Some(again));
    }

    #[test]
    fn a_newer_overlay_is_refused_and_left_alone() {
        let home = Home::new();
        let dir = home.overlay("box", MDB_GENERATION + 1);
        let hash = hash_machine_dir(&dir).unwrap();
        let e = home.mdb.load("box").unwrap_err();
        let refusal = overlay_error(&e);
        assert_eq!((refusal.found, refusal.want), (MDB_GENERATION + 1, MDB_GENERATION));
        assert_eq!(refusal.why, Why::Newer { kept: None });
        let text = e.to_string();
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(text.contains("\"box\"") && text.contains("cactup update"), "{text}");
        assert_eq!(refusal.brief(), format!("needs MDB generation {}; run `cactup update`", MDB_GENERATION + 1));
        assert_eq!(hash_machine_dir(&dir).unwrap(), hash);
        assert!(!home.backups().exists());

        // When a newer cactup kept this generation's copy, the refusal says where.
        let kept = home.backups().join("box").join(format!("generation-{MDB_GENERATION}"));
        fs::create_dir_all(&kept).unwrap();
        let e = home.mdb.load("box").unwrap_err();
        let text = e.to_string();
        assert_eq!(text.lines().count(), 2, "{text}");
        assert!(text.contains(&kept.display().to_string()), "{text}");
        assert_eq!(hash_machine_dir(&dir).unwrap(), hash);
    }

    /// The shape of a refusal of an older overlay: one headline naming the
    /// machine, both generations and why; then the hand fix (one generation
    /// section, the file to edit) and the fresh start (which loses edits),
    /// each on its own line. No spec references.
    fn assert_older_refusal_shape(text: &str, name: &str, found: u32, next: u32) {
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(lines[0].contains(&format!("\"{name}\"")), "{text}");
        assert!(lines[0].contains(&format!("generation {found}")), "{text}");
        assert!(lines[0].contains(&format!("reads {MDB_GENERATION}")), "{text}");
        assert!(lines[1].contains(&format!("\"Generation {next}\"")), "{text}");
        assert!(lines[1].contains(&format!("mdb-generation = {next}")), "{text}");
        assert!(lines[1].contains(&format!("{name}/meta.toml")), "{text}");
        // Starting over keeps the old files: moved aside, never deleted.
        assert!(lines[2].contains("without your edits") && lines[2].contains("moved aside"), "{text}");
        assert!(lines[2].contains("&& mv '") && !lines[2].contains("delete"), "{text}");
        assert!(lines[2].contains(&format!("cactup machine create {name}")), "{text}");
        assert!(!text.contains('§'), "{text}");
    }

    #[test]
    fn a_generation_1_overlay_is_refused_untouched() {
        let home = Home::new();
        let dir = home.overlay("old", 1);
        let hash = hash_machine_dir(&dir).unwrap();
        let e = home.mdb.load("old").unwrap_err();
        assert_eq!(overlay_error(&e).why, Why::Unsupported);
        assert_older_refusal_shape(&e.to_string(), "old", 1, 2);
        assert!(e.to_string().contains("does not update automatically"), "{e}");
        let brief = overlay_error(&e).brief();
        assert!(!brief.contains('\n') && brief.contains("cactup machine show old"), "{brief}");
        // Refused before anything was written: no backup, no lock left.
        assert_eq!(hash_machine_dir(&dir).unwrap(), hash);
        assert!(!home.backups().exists());
        assert!(!home.mdb.user_root.join(".old.migrate.lock").exists());
    }

    #[test]
    fn a_generation_2_overlay_waits_for_its_step() {
        let home = Home::new();
        let dir = home.overlay("two", 2);
        let hash = hash_machine_dir(&dir).unwrap();
        let e = home.mdb.load("two").unwrap_err();
        let text = e.to_string();
        assert!(text.contains("not implemented yet"), "{text}");
        assert_older_refusal_shape(&text, "two", 2, 3);
        assert_eq!(hash_machine_dir(&dir).unwrap(), hash);
        assert!(!home.backups().exists());
    }

    #[test]
    fn start_over_names_the_base_an_overlay_came_from_while_it_exists() {
        let home = Home::new();
        let dir = home.overlay("old", 1);
        let meta = fs::read_to_string(dir.join("meta.toml")).unwrap();
        let with_origin = |base: &str| format!("{meta}origin = {{ from = \"{base}\", hash = \"0\" }}\n");
        fs::write(dir.join("meta.toml"), with_origin("qbd")).unwrap();
        let text = home.mdb.load("old").unwrap_err().to_string();
        assert!(text.ends_with("cactup machine create old --from-existing qbd"), "{text}");
        let aside = home.backups().join("old/before-start-over");
        assert!(text.contains(&format!("mv '{}' '{}'", dir.display(), aside.display())), "{text}");

        // Another overlay as the base counts while it is there.
        home.overlay("mine", MDB_GENERATION);
        fs::write(dir.join("meta.toml"), with_origin("mine")).unwrap();
        let text = home.mdb.load("old").unwrap_err().to_string();
        assert!(text.ends_with("--from-existing mine"), "{text}");

        // A base that is gone (retired, or the overlay itself) offers no
        // fresh start: following it would only lose the machine.
        for base in ["retired-machine", "old"] {
            fs::write(dir.join("meta.toml"), with_origin(base)).unwrap();
            let text = home.mdb.load("old").unwrap_err().to_string();
            assert_eq!(text.lines().count(), 2, "{base}: {text}");
            assert!(!text.contains("start over"), "{base}: {text}");
        }
    }

    /// The shape of a transient failure: what happened and to retry, with
    /// no hand fix and no fresh start (a hand edit now would be a second
    /// writer).
    fn assert_transient_shape(e: &anyhow::Error, name: &str, mentions: &str) {
        let refusal = overlay_error(e);
        assert!(matches!(refusal.why, Why::Transient { .. }), "{e}");
        let text = e.to_string();
        assert_eq!(text.lines().count(), 2, "{text}");
        assert!(text.contains(&format!("\"{name}\"")) && text.contains("just now") && text.contains(mentions), "{text}");
        assert!(text.contains("run the command again"), "{text}");
        assert!(!text.contains("To fix by hand") && !text.contains("start over"), "{text}");
        assert!(refusal.brief().contains("could not be updated just now"), "{}", refusal.brief());
    }

    #[test]
    fn a_lock_held_too_long_is_transient() {
        let home = Home::new();
        let dir = home.overlay("box", MDB_GENERATION - 1);
        let hash = hash_machine_dir(&dir).unwrap();
        let lock_path = home.mdb.user_root.join(".box.migrate.lock");
        let _held = LinkLock::acquire(&lock_path).unwrap();
        LOCK_WAIT_HERE.with(|w| w.set(Some(Duration::from_millis(200))));
        let result = home.mdb.load("box");
        LOCK_WAIT_HERE.with(|w| w.set(None));
        assert_transient_shape(&result.unwrap_err(), "box", ".box.migrate.lock");
        assert_eq!(hash_machine_dir(&dir).unwrap(), hash);
    }

    #[test]
    fn a_lock_that_cannot_be_taken_is_transient() {
        use std::os::unix::fs::PermissionsExt;
        let home = Home::new();
        let dir = home.overlay("box", MDB_GENERATION - 1);
        home.overlay("old", 1);
        let hash = hash_machine_dir(&dir).unwrap();
        // A read-only user layer (a full quota fails the same way).
        fs::set_permissions(&home.mdb.user_root, fs::Permissions::from_mode(0o555)).unwrap();
        if fs::write(home.mdb.user_root.join(".probe"), "").is_ok() {
            eprintln!("skipping: permissions do not bind here (root?)");
            return;
        }
        let result = home.mdb.load("box");
        // A refusal that needs no lock is still the real verdict.
        let old = home.mdb.load("old");
        fs::set_permissions(&home.mdb.user_root, fs::Permissions::from_mode(0o755)).unwrap();
        assert_transient_shape(&result.unwrap_err(), "box", "Permission denied");
        assert_eq!(overlay_error(&old.unwrap_err()).why, Why::Unsupported);
        assert_eq!(hash_machine_dir(&dir).unwrap(), hash);
    }

    #[test]
    fn a_failed_record_after_the_commit_is_only_a_warning() {
        let home = Home::new();
        let dir = home.overlay("box", MDB_GENERATION - 1);
        FAIL_FINISH.with(|f| f.set(true));
        let result = home.mdb.load("box");
        FAIL_FINISH.with(|f| f.set(false));
        result.unwrap();
        assert_eq!(recorded(&fs::read_to_string(dir.join("meta.toml")).unwrap()), Recorded::At(MDB_GENERATION));
        home.mdb.load("box").unwrap();
    }

    #[test]
    fn a_copy_from_a_run_that_never_committed_is_replaced() {
        let home = Home::new();
        let dir = home.overlay("box", MDB_GENERATION - 1);
        // Left by a run stopped between its backup and its commit, from
        // files the user has edited since.
        let stale = home.backups().join("box").join(format!("generation-{}", MDB_GENERATION - 1));
        fs::create_dir_all(&stale).unwrap();
        fs::write(stale.join("meta.toml"), "# an older edit\n").unwrap();
        fs::write(stale.join("leftover"), "x").unwrap();
        let before = fs::read_to_string(dir.join("meta.toml")).unwrap();
        home.mdb.load("box").unwrap();
        assert_eq!(fs::read_to_string(stale.join("meta.toml")).unwrap(), before);
        assert!(!stale.join("leftover").exists());
        assert!(stale.join(".complete").is_file());
    }

    #[test]
    fn a_result_that_would_not_load_is_never_written() {
        let home = Home::new();
        let dir = home.overlay("bad", MDB_GENERATION - 1);
        // A routed script whose file is missing: today's validation refuses.
        fs::remove_file(dir.join("runscripts/default.sh")).unwrap();
        let hash = hash_machine_dir(&dir).unwrap();
        let e = home.mdb.load("bad").unwrap_err();
        let text = e.to_string();
        assert!(text.contains("would not load") && text.contains("default"), "{text}");
        assert_older_refusal_shape(&text, "bad", MDB_GENERATION - 1, MDB_GENERATION);
        assert_eq!(hash_machine_dir(&dir).unwrap(), hash);
        assert!(!home.backups().exists());
    }

    /// A step that edits an optionlist, idempotently, for the commit tests.
    fn mark_optionlists(overlay: &mut Overlay) -> Result<Vec<String>, Stop> {
        for file in &mut overlay.optionlists {
            file.doc["cactup"]["description"] = toml_edit::value("migrated");
        }
        Ok(vec!["marked".to_owned()])
    }

    fn refuse_here(_: &mut Overlay) -> Result<Vec<String>, Stop> {
        Err(Stop::Found("it found something".to_owned()))
    }

    #[test]
    fn an_interrupted_commit_is_finished_by_the_next_load() {
        let home = Home::new();
        let want = MDB_GENERATION;
        let dir = home.overlay("box", want - 1);
        fs::write(dir.join("optionlists/other.toml"), "[options]\nVERSION = \"2\"\n").unwrap();
        let original = fs::read_to_string(dir.join("optionlists/default.toml")).unwrap();
        let mut steps: Vec<Step> = (1..want - 1).map(|_| Step { run: raise, optional: false }).collect();
        steps.push(Step { run: mark_optionlists, optional: false });
        let backups = home.backups();
        let place = home.place("box", &dir, &backups);

        FAULT_AFTER.with(|f| f.set(Some(1)));
        let result = bring_forward_with(&place, want - 1, &steps, want);
        FAULT_AFTER.with(|f| f.set(None));
        assert!(format!("{:#}", result.unwrap_err()).contains("injected fault"));
        // One optionlist written, the other and meta.toml not: still the old
        // generation on disk, and the backup is not marked complete.
        assert!(fs::read_to_string(dir.join("optionlists/default.toml")).unwrap().contains("migrated"));
        assert!(!fs::read_to_string(dir.join("optionlists/other.toml")).unwrap().contains("migrated"));
        assert_eq!(recorded(&fs::read_to_string(dir.join("meta.toml")).unwrap()), Recorded::At(want - 1));
        let backup = backups.join("box").join(format!("generation-{}", want - 1));
        assert!(!backup.join(".complete").exists());
        assert!(backup.join(COMMITTING).exists());

        bring_forward_with(&place, want - 1, &steps, want).unwrap();
        assert_eq!(recorded(&fs::read_to_string(dir.join("meta.toml")).unwrap()), Recorded::At(want));
        for file in ["default", "other"] {
            let text = fs::read_to_string(dir.join(format!("optionlists/{file}.toml"))).unwrap();
            assert_eq!(text.matches("migrated").count(), 1, "{file}: {text}");
        }
        // The interrupted run's copy was reused: it holds the original files.
        assert!(backup.join(".complete").is_file());
        assert_eq!(fs::read_to_string(backup.join("optionlists/default.toml")).unwrap(), original);
        assert_eq!(fs::read_dir(backups.join("box")).unwrap().count(), 1);
        assert!(fs::read_to_string(backup.join("NOTES.txt")).unwrap().contains("marked"));
    }

    #[test]
    fn a_later_stop_keeps_the_steps_already_done() {
        let home = Home::new();
        let dir = home.overlay("box", 1);
        let steps = [Step { run: mark_optionlists, optional: false }, Step { run: refuse_here, optional: false }];
        let backups = home.backups();
        let place = home.place("box", &dir, &backups);
        let e = bring_forward_with(&place, 1, &steps, 3).unwrap_err();
        let refusal = overlay_error(&e);
        assert_eq!(refusal.found, 2);
        assert_eq!(refusal.why, Why::Stopped { finding: "it found something".to_owned(), from: Some(1) });
        let text = e.to_string();
        assert!(text.contains("updated from MDB generation 1 to 2") && text.contains("it found something"), "{text}");
        assert!(text.contains("mdb-generation = 3"), "{text}");
        assert!(refusal.brief().contains("updated from 1"), "{}", refusal.brief());
        // The finished step is on disk, stamped with the generation it reached.
        assert_eq!(recorded(&fs::read_to_string(dir.join("meta.toml")).unwrap()), Recorded::At(2));
        assert!(fs::read_to_string(dir.join("optionlists/default.toml")).unwrap().contains("migrated"));
        let notes = fs::read_to_string(backups.join("box/generation-1/NOTES.txt")).unwrap();
        assert!(notes.contains("Stopped at generation 2: it found something"), "{notes}");
    }

    #[test]
    fn a_compute_node_never_rewrites_an_overlay() {
        let home = Home::new();
        let dir = home.overlay("box", MDB_GENERATION - 1);
        let hash = hash_machine_dir(&dir).unwrap();
        COMPUTE_NODE_HERE.with(|c| c.set(true));
        let result = home.mdb.load("box");
        COMPUTE_NODE_HERE.with(|c| c.set(false));
        let e = result.unwrap_err();
        assert!(matches!(overlay_error(&e).why, Why::Stopped { from: None, .. }), "{e}");
        assert_eq!(hash_machine_dir(&dir).unwrap(), hash);
        assert!(!home.backups().exists());
    }

    #[test]
    fn a_migration_waits_for_another_holding_the_lock() {
        let home = Home::new();
        let dir = home.overlay("box", MDB_GENERATION - 1);
        let lock = LinkLock::acquire(&home.mdb.user_root.join(".box.migrate.lock")).unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1200));
            drop(lock);
        });
        home.mdb.load("box").unwrap();
        releaser.join().unwrap();
        assert_eq!(recorded(&fs::read_to_string(dir.join("meta.toml")).unwrap()), Recorded::At(MDB_GENERATION));
    }

    #[test]
    fn files_keep_their_mode() {
        use std::os::unix::fs::PermissionsExt;
        let home = Home::new();
        let dir = home.overlay("box", MDB_GENERATION - 1);
        fs::set_permissions(dir.join("meta.toml"), fs::Permissions::from_mode(0o640)).unwrap();
        home.mdb.load("box").unwrap();
        let mode = fs::metadata(dir.join("meta.toml")).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o640);
    }

    #[test]
    fn generation_markers_of_unmarked_overlays() {
        assert_eq!(looks_like("[variants.submitscript]\n"), Some(1));
        assert_eq!(looks_like("[environment]\nenv-build-setup = \"x\"\n"), Some(2));
        assert_eq!(looks_like("[universes.u]\nenv-run-setup = \"x\"\n"), Some(2));
        assert_eq!(looks_like("[universes.u]\nbuild-env-setup = \"x\"\n"), None);
        assert_eq!(looks_like("[machine]\n"), None);
    }

    #[test]
    fn a_published_guide_points_at_the_generations_anchor() {
        assert_eq!(guide_section(GENERATIONS_URL, 3), format!("{GENERATIONS_URL}#generation-3"));
        assert_eq!(guide_section("/m/GENERATIONS.md", 3), "\"Generation 3\" in '/m/GENERATIONS.md'");
    }

    // -----------------------------------------------------------------------
    // Snapshots: every generation's real `machine create` output must come
    // forward (fixtures/mdb-overlays, plan §6).

    fn fixtures() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/mdb-overlays")
    }

    /// Generations whose step is still a placeholder that refuses with "not
    /// implemented yet". Plan phase P2 replaces the 2→3 one and empties this.
    const PLACEHOLDERS: &[u32] = &[2];

    /// The machine directories (holding a meta.toml) directly under `dir`.
    fn machine_dirs(dir: &Path) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = match fs::read_dir(dir) {
            Ok(entries) => entries.flatten().map(|e| e.path()).filter(|p| p.join("meta.toml").is_file()).collect(),
            Err(_) => Vec::new(),
        };
        dirs.sort();
        dirs
    }

    /// Copy the overlay at `source` into a scratch home as `name`, minus the
    /// test's own `expect.toml`.
    fn install(home: &Home, source: &Path, name: &str) -> PathBuf {
        let dir = home.dir(name);
        fs::create_dir_all(&dir).unwrap();
        copy_tree(source, &dir).unwrap();
        let _ = fs::remove_file(dir.join("expect.toml"));
        dir
    }

    fn generation_of(dir: &Path) -> u32 {
        match recorded(&fs::read_to_string(dir.join("meta.toml")).unwrap()) {
            Recorded::At(n) => n,
            other => panic!("'{}' records no generation ({other:?})", dir.display()),
        }
    }

    /// Load `name`, which must be refused before anything is written.
    fn assert_refused(home: &Home, name: &str, generation: u32) -> String {
        let dir = home.dir(name);
        let hash = hash_machine_dir(&dir).unwrap();
        let e = home.mdb.load(name).expect_err(name);
        let refusal = overlay_error(&e);
        if generation == 1 {
            assert_eq!(refusal.why, Why::Unsupported, "{name}: {e}");
        } else if PLACEHOLDERS.contains(&generation) {
            assert!(e.to_string().contains("not implemented yet"), "{name}: {e}");
        }
        assert_eq!(hash_machine_dir(&dir).unwrap(), hash, "{name}: a refusal wrote to the overlay");
        e.to_string()
    }

    /// Load `name`, which must come forward and then stay put.
    fn assert_migrates(home: &Home, name: &str, generation: u32) {
        let dir = home.dir(name);
        home.mdb.load(name).unwrap_or_else(|e| panic!("{name} (generation {generation}) did not migrate: {e:#}"));
        assert_eq!(generation_of(&dir), MDB_GENERATION, "{name}");
        let backup = home.backups().join(name).join(format!("generation-{generation}"));
        assert!(backup.join(".complete").is_file(), "{name}: no finished backup");
        // Loads again without writing anything.
        let hash = hash_machine_dir(&dir).unwrap();
        home.mdb.load(name).unwrap();
        assert_eq!(hash_machine_dir(&dir).unwrap(), hash, "{name}: the second load wrote");
        // Every flavor's whole optionlist parses, not just its header.
        let machine = home.mdb.load(name).unwrap();
        for flavor in machine.flavors().unwrap() {
            optionlist::Optionlist::load(&machine.optionlist_path(&flavor))
                .unwrap_or_else(|e| panic!("{name} flavor {flavor}: {e:#}"));
        }
    }

    /// The snapshots must exist for every older generation: a bump without
    /// one fails here.
    #[test]
    fn every_older_generation_has_snapshots() {
        let missing: Vec<u32> = (1..MDB_GENERATION)
            .filter(|n| machine_dirs(&fixtures().join(format!("gen-{n}"))).is_empty())
            .collect();
        assert!(
            missing.is_empty(),
            "fixtures/mdb-overlays has no overlay snapshots for generation(s) {missing:?}; take them with \
             ci/overlay-fixtures.sh N on the tree before the bump"
        );
    }

    #[test]
    fn every_snapshot_migrates() {
        for n in 1..MDB_GENERATION {
            for source in machine_dirs(&fixtures().join(format!("gen-{n}"))) {
                let name = source.file_name().unwrap().to_string_lossy().into_owned();
                let home = Home::new();
                let dir = install(&home, &source, &name);
                assert_eq!(generation_of(&dir), n, "{}", source.display());
                if n == 1 || PLACEHOLDERS.contains(&n) {
                    assert_refused(&home, &name, n);
                    continue;
                }
                let before: Vec<(PathBuf, Vec<u8>)> = walk(&dir).into_iter().map(|p| (p.clone(), fs::read(&p).unwrap())).collect();
                assert_migrates(&home, &name, n);
                // Raising the number changes that line and nothing else.
                for (path, bytes) in before {
                    let now = fs::read(&path).unwrap();
                    if path.ends_with("meta.toml") {
                        let (old, new) = (String::from_utf8(bytes).unwrap(), String::from_utf8(now).unwrap());
                        let differing: Vec<_> = old.lines().zip(new.lines()).filter(|(a, b)| a != b).collect();
                        assert!(
                            differing.len() == 1 && differing[0].1.contains("mdb-generation"),
                            "gen-{n}/{name}: {differing:?}"
                        );
                    } else {
                        assert_eq!(now, bytes, "gen-{n}/{name}: '{}' changed", path.display());
                    }
                }
            }
        }
        for case in machine_dirs(&fixtures().join("edge")) {
            check_edge_case(&case);
        }
    }

    /// Every file under `dir` with its bytes.
    fn contents(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut files: Vec<_> = walk(dir).into_iter().map(|p| (p.clone(), fs::read(&p).unwrap())).collect();
        files.sort();
        files
    }

    /// One `edge/<case>/`: the case directory is the overlay, named after
    /// the case, and its `expect.toml` says what loading it must do
    /// (fixtures/mdb-overlays/README.md). `findings` are kind strings; each
    /// is checked below, and an unknown one fails the test.
    fn check_edge_case(case: &Path) {
        let expect_path = case.join("expect.toml");
        let expect: toml::Table = fs::read_to_string(&expect_path)
            .unwrap_or_else(|e| panic!("'{}': {e}", expect_path.display()))
            .parse()
            .unwrap_or_else(|e| panic!("'{}': {e}", expect_path.display()));
        let name = case.file_name().unwrap().to_string_lossy().into_owned();
        let label = format!("edge/{name}");
        let home = Home::new();
        let dir = install(&home, case, &name);
        let outcome = expect.get("outcome").and_then(|v| v.as_str()).unwrap_or_else(|| panic!("{label}: no outcome"));
        let result_generation = expect
            .get("result-generation")
            .and_then(|v| v.as_integer())
            .unwrap_or_else(|| panic!("{label}: no result-generation"));
        let findings: Vec<&str> = expect
            .get("findings")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        let generation_now = |dir: &Path| match recorded(&fs::read_to_string(dir.join("meta.toml")).unwrap()) {
            Recorded::At(n) => i64::from(n),
            Recorded::Missing => 0,
            Recorded::Unreadable => panic!("{label}: unreadable generation"),
        };
        let start = generation_now(&dir);
        let before = contents(&dir);

        // A placeholder step refuses whatever the case expects of the real
        // one, before writing anything.
        if (1..MDB_GENERATION).any(|n| PLACEHOLDERS.contains(&n) && i64::from(n) == start) {
            assert_refused(&home, &name, start as u32);
            return;
        }
        let result = home.mdb.load(&name);
        assert_eq!(generation_now(&dir), result_generation, "{label}: generation after the load");
        let after = contents(&dir);
        // What the user reads: the refusal, or the backup's notes.
        let message = match (&result, outcome) {
            (Ok(_), "migrated") => {
                let backup = home.backups().join(&name).join(format!("generation-{start}"));
                assert!(backup.join(".complete").is_file(), "{label}: no finished backup");
                fs::read_to_string(backup.join("NOTES.txt")).unwrap()
            }
            (Ok(_), "untouched") => {
                assert_eq!(after, before, "{label}: an untouched overlay was written");
                assert!(!home.backups().exists(), "{label}");
                String::new()
            }
            (Err(e), "refused") => {
                let _ = overlay_error(e);
                e.to_string()
            }
            (Ok(_), other) => panic!("{label}: loaded, but the expected outcome is {other:?}"),
            (Err(e), other) => panic!("{label}: refused, but the expected outcome is {other:?}: {e:#}"),
        };
        let error = result.as_ref().err().map(overlay_error);
        let changed = |pick: &dyn Fn(&Path) -> bool| -> Vec<PathBuf> {
            before
                .iter()
                .zip(&after)
                .filter(|((path, old), (_, new))| pick(path) && old != new)
                .map(|((path, _), _)| path.clone())
                .collect()
        };
        assert_eq!(before.len(), after.len(), "{label}: files appeared or vanished");
        let comments = |files: &[(PathBuf, Vec<u8>)]| -> Vec<String> {
            files
                .iter()
                .filter(|(path, _)| path.extension().is_some_and(|e| e == "toml"))
                .flat_map(|(_, bytes)| String::from_utf8_lossy(bytes).lines().map(str::to_owned).collect::<Vec<_>>())
                .filter_map(|line| line.find('#').map(|at| line[at..].trim().to_owned()))
                .collect()
        };
        for finding in findings {
            match finding.split_once(':') {
                _ if finding == "generation-1-not-migrated" => {
                    assert_eq!(error.map(|e| &e.why), Some(&Why::Unsupported), "{label}");
                }
                _ if finding == "overlay-newer-than-binary" => {
                    assert!(matches!(error.map(|e| &e.why), Some(Why::Newer { .. })), "{label}");
                }
                _ if finding == "files-untouched" => assert_eq!(after, before, "{label}"),
                _ if finding == "no-recorded-generation-warning" => {
                    assert!(home.mdb.load(&name).unwrap().meta.cactup.mdb_generation.is_none(), "{label}");
                }
                _ if finding == "generation-number-only" => {
                    let meta = |p: &Path| p.ends_with("meta.toml") && p.parent() == Some(dir.as_path());
                    assert!(changed(&|p| !meta(p)).is_empty(), "{label}: {:?}", changed(&|p| !meta(p)));
                    let (old, new) = (&before.iter().find(|(p, _)| meta(p)).unwrap().1, &after.iter().find(|(p, _)| meta(p)).unwrap().1);
                    let (old, new) = (String::from_utf8_lossy(old), String::from_utf8_lossy(new));
                    let differing: Vec<_> = old.lines().zip(new.lines()).filter(|(a, b)| a != b).collect();
                    assert!(differing.len() == 1 && differing[0].1.contains("mdb-generation"), "{label}: {differing:?}");
                }
                _ if finding == "scripts-untouched" => {
                    let script = |p: &Path| {
                        let rel = p.strip_prefix(&dir).unwrap();
                        rel.components().count() > 1 && !rel.starts_with("optionlists")
                    };
                    assert!(changed(&script).is_empty(), "{label}: {:?}", changed(&script));
                }
                _ if finding == "comments-preserved" => {
                    let (old, new) = (comments(&before), comments(&after));
                    for comment in &old {
                        assert!(new.contains(comment), "{label}: comment {comment:?} lost");
                    }
                }
                // "renamed:<old>-><new>": the old key is gone, the new one there.
                Some(("renamed", rename)) => {
                    let (old, new) = rename.split_once("->").unwrap_or_else(|| panic!("{label}: {finding}"));
                    let meta = fs::read_to_string(dir.join("meta.toml")).unwrap();
                    let keys: Vec<&str> = meta.lines().filter_map(|l| l.split_once('=')).map(|(k, _)| k.trim()).collect();
                    assert!(!keys.contains(&old) && keys.contains(&new), "{label}: {finding}\n{meta}");
                }
                // The details of a note or refusal ("build-universe:et-sing")
                // and the key it names ("environment.env-build-setup"): the
                // words of the message.
                Some((_, detail)) => assert!(message.contains(detail), "{label}: {finding:?} not in\n{message}"),
                None if finding.contains('.') => {
                    let key = finding.rsplit('.').next().unwrap();
                    assert!(message.contains(key), "{label}: {finding:?} not in\n{message}");
                }
                None => {
                    let word = match finding {
                        "may-belong-in-provision" => "provision",
                        "old-and-new-key-both-present" => "both",
                        "py-env-setup-differs" => "ENV_SETUP",
                        other => panic!("{label}: unknown finding kind {other:?}; teach check_edge_case it"),
                    };
                    assert!(message.contains(word), "{label}: {finding:?} ({word:?}) not in\n{message}");
                }
            }
        }
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path);
            }
        }
        out
    }
}
