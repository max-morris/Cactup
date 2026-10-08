//! `cactup cache report` (§18.5): what a build recorded about its compiles
//! (`build-cache = record`), and how much of it another build's cache
//! entries would have served.
//!
//! The cache serves nothing yet. This is the measurement that says what it
//! would be worth: how many compiles are keyed at all, what keying costs,
//! and — comparing two builds — how many keys they share and, where they do
//! not, which part of the key differs.

use crate::args::CacheCommand;
use crate::build::attempt::BuildAttempt;
use crate::commands::Ctx;
use crate::installation::Installation;
use crate::objcache::event::{self, Event, Outcome};
use crate::objcache::{events_path, upkeep};
use crate::Res;
use anyhow::{anyhow, bail};
use colored::Colorize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn dispatch(ctx: &Ctx, command: CacheCommand) -> Res<()> {
    match command {
        CacheCommand::Report { name, attempt, against, against_installation, against_attempt, long } => {
            let installation = Installation::resolve(ctx)?;
            let name = match name {
                Some(name) => name,
                None => installation.meta()?.active_config()?.to_owned(),
            };
            // Both builds are found before anything is printed: a report
            // that ends in an error is not half a report.
            let build = Recorded::open(&installation, &name, attempt, None)?;
            let compare = against.is_some() || against_installation.is_some() || against_attempt.is_some();
            let other = match compare {
                false => None,
                true => {
                    let other_installation = match against_installation {
                        Some(alias) => named_installation(ctx, &alias)?,
                        None => Installation::new(installation.alias.clone(), installation.root.clone()),
                    };
                    let other_name = against.unwrap_or_else(|| name.clone());
                    // Compared with itself, a build serves everything: the
                    // build to compare with is another one.
                    let same_config = other_installation.root == installation.root && other_name == name;
                    if same_config && against_attempt == Some(build.attempt) {
                        bail!(
                            "build attempt {:04} of config \"{name}\" would be compared with itself; name another \
                             attempt, config or installation to compare against",
                            build.attempt
                        );
                    }
                    Some(Recorded::open(&other_installation, &other_name, against_attempt, same_config.then_some(build.attempt))?)
                }
            };
            println!("{}", build.title().bold());
            print!("{}{}", build.unreadable_note("this"), summary(&build.events));
            if let Some(other) = other {
                println!("\n{}", format!("against {}", other.title()).bold());
                print!("{}{}", other.unreadable_note("that"), comparison(&build.events, &other.events, long));
            }
            Ok(())
        }
        CacheCommand::Stats => {
            let (root, from) = store_root(ctx)?;
            stats(&root, &from)
        }
        CacheCommand::Gc { unused_for, to_size, dry_run } => {
            if unused_for.is_none() && to_size.is_none() {
                bail!("say what to remove: --unused-for <age>, --to-size <size>, or both");
            }
            let unused_for = unused_for.as_deref().map(upkeep::parse_age).transpose()?;
            let to_size = to_size.as_deref().map(upkeep::parse_size).transpose()?;
            let (root, from) = store_root(ctx)?;
            println!("The build cache in {} ({from}).", root.display());
            gc(&root, unused_for, to_size, dry_run)
        }
        CacheCommand::Verify => {
            let (root, from) = store_root(ctx)?;
            println!("The build cache in {} ({from}).", root.display());
            verify(&root)
        }
    }
}

/// The store's root, as a build on this machine would have it, and where
/// that came from (`objcache::store_root`).
fn store_root(ctx: &Ctx) -> Res<(PathBuf, String)> {
    let db = ctx.db.read()?;
    // The machine is only asked for its places: a host it cannot resolve,
    // or does not recognize (a build there uses `generic`, which sets none),
    // leaves the knobs and the default.
    let machine = crate::commands::machine::resolve_quietly(ctx).ok().flatten();
    let root = crate::objcache::store_root(&db, machine.as_ref());
    for warning in &root.warnings {
        println!("{} {warning}; looking further for the build cache", "warning:".yellow().bold());
    }
    Ok((root.path, root.from))
}

/// When, as people read it: so many days ago.
fn ago(now: SystemTime, then: SystemTime) -> String {
    let days = now.duration_since(then).unwrap_or_default().as_secs() / (24 * 3600);
    match days {
        0 => "today".to_owned(),
        1 => "1 day ago".to_owned(),
        n => format!("{n} days ago"),
    }
}

/// `cactup cache stats` (§18.9).
fn stats(root: &Path, from: &str) -> Res<()> {
    if !root.is_dir() {
        println!("The build cache in {} ({from}) is empty: nothing has been stored there yet.", root.display());
        return Ok(());
    }
    let scan = upkeep::scan(root)?;
    // The fileserver's clock where this user can write in the store; ages
    // shown in days need nothing finer than this host's, where not.
    let now = crate::lock::fileserver_now(root).unwrap_or_else(|_| SystemTime::now());
    println!("{}", format!("build cache in {} ({from})", root.display()).bold());
    for (log, why) in &scan.unreadable_logs {
        println!(
            "  {} use log {} cannot be read ({why}): `cactup cache gc` will not run until it can",
            "warning:".yellow().bold(),
            log.display()
        );
    }
    if scan.machines.is_empty() {
        println!("  nothing stored yet");
    }
    let day = Duration::from_secs(24 * 3600);
    for machine in &scan.machines {
        let bytes: u64 = machine.entries.iter().map(|e| e.bytes).sum();
        println!("  {}: {} objects, {}", machine.name.bold(), machine.entries.len(), upkeep::human(bytes));
        let last_uses = machine.last_uses();
        let last = |e: &upkeep::Entry| last_uses.get(e.key.as_str()).map_or(e.published, |used| (*used).max(e.published));
        if let (Some(oldest), Some(newest)) =
            (machine.entries.iter().map(|e| e.published).min(), machine.entries.iter().map(|e| e.published).max())
        {
            match (ago(now, oldest), ago(now, newest)) {
                (oldest, newest) if oldest == newest => println!("    all stored {oldest}"),
                (oldest, newest) => println!("    stored between {oldest} and {newest}"),
            }
        }
        for days in [7, 30] {
            let since = now.checked_sub(day * days).unwrap_or(UNIX_EPOCH);
            let used = machine.entries.iter().filter(|e| last(e) >= since).count();
            println!("    used or stored in the last {days} days: {used}");
        }
        if !machine.temps.is_empty() {
            println!("    left over from interrupted builds: {} files (cactup cache gc removes those a day old)", machine.temps.len());
        }
    }
    for dir in &scan.other_formats {
        println!(
            "  {} holds objects in another version of cactup's format, which this cactup neither reads nor removes; \
             once no older cactup builds with this cache, remove it with: rm -r {}",
            dir.display(),
            dir.display()
        );
    }
    let total = scan.bytes();
    println!("  in all: {}", upkeep::human(total));
    // Best-effort: a store this user cannot write in can still be looked at.
    let _ = upkeep::stamp_size(root, total);
    Ok(())
}

/// `cactup cache gc` (§18.9).
fn gc(root: &Path, unused_for: Option<Duration>, to_size: Option<u64>, dry_run: bool) -> Res<()> {
    if !root.is_dir() {
        println!("The build cache in {} is empty: nothing to remove.", root.display());
        return Ok(());
    }
    // One at a time per store, across hosts.
    let _lock = crate::lock::LinkLock::acquire(&root.join("gc.lock"))?.with_heartbeat();
    let scan = upkeep::scan(root)?;
    // What a log that cannot be read records as in use is not known: no
    // entry can be judged unused.
    if let Some((log, why)) = scan.unreadable_logs.first() {
        bail!(
            "the use log {} cannot be read ({why}), so what it records as in use is not known; \
             nothing was removed (make it readable, or remove it if that is right, and run gc again)",
            log.display()
        );
    }
    let now = crate::lock::fileserver_now(root)?;
    let plan = upkeep::plan(&scan, now, unused_for, to_size);
    let bytes: u64 = plan.entries.iter().map(|(e, _)| e.bytes).sum();
    if dry_run {
        for (entry, last) in &plan.entries {
            println!("  would remove {} ({}, last used or stored {})", entry.path.display(), upkeep::human(entry.bytes), ago(now, *last));
        }
        println!(
            "Would remove {} of {} objects ({}), leaving {}, and {} files left over from interrupted builds.",
            plan.entries.len(),
            scan.machines.iter().map(|m| m.entries.len()).sum::<usize>(),
            upkeep::human(bytes),
            upkeep::human(plan.bytes_after),
            plan.temps.len()
        );
        return Ok(());
    }
    let done = upkeep::carry_out(&plan)?;
    upkeep::stamp_size(root, plan.bytes_after)?;
    println!(
        "Removed {} objects ({}) and {} files left over from interrupted builds; the build cache holds {}.",
        done.entries,
        upkeep::human(done.bytes),
        done.temps,
        upkeep::human(plan.bytes_after)
    );
    Ok(())
}

/// `cactup cache verify` (§18.9).
fn verify(root: &Path) -> Res<()> {
    use crate::objcache::store::{Miss, Store};
    if !root.is_dir() {
        println!("The build cache in {} is empty: nothing to verify.", root.display());
        return Ok(());
    }
    let scan = upkeep::scan(root)?;
    let mut checks = Vec::new();
    for machine in &scan.machines {
        let store = Store::new(root, &machine.name)?;
        checks.extend(machine.entries.iter().map(|entry| (store.clone(), entry.key.clone())));
    }
    // Counted as it goes, so that an interrupted run can say what it did.
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    let (good, removed, damaged, foreign, unreadable) = (AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0));
    let (progress, renderer) = crate::manifest::setup_prodash_if_tty();
    let checking = progress.add_child("check the build cache");
    checking.init(Some(checks.len()), Some(prodash::unit::label("objects")));
    let checking = std::sync::Mutex::new(checking);
    let ran = crate::par::parallel_map(&checks, |(store, key)| {
        let counter = match store.check(key) {
            Ok(()) => &good,
            Err(Miss::Invalid { removed: true, .. }) => &removed,
            Err(Miss::Invalid { removed: false, .. }) => &damaged,
            Err(Miss::Foreign(_)) => &foreign,
            Err(Miss::Absent) => return,
            Err(_) => &unreadable,
        };
        counter.fetch_add(1, Relaxed);
        checking.lock().expect("progress poisoned").inc();
    });
    drop(checking);
    if let Some(renderer) = renderer {
        renderer.shutdown_and_wait();
    }
    let removed = removed.load(Relaxed);
    if let Err(e) = ran {
        bail!("{e:#}, having removed {removed} damaged objects");
    }
    println!("{} objects are whole; {removed} were damaged and are removed.", good.load(Relaxed));
    let damaged = damaged.load(Relaxed);
    if damaged > 0 {
        println!("{damaged} are damaged and could not be removed (no permission, or replaced meanwhile).");
    }
    let foreign = foreign.load(Relaxed);
    if foreign > 0 {
        println!("{foreign} were stored by another version of cactup, and are left as they are.");
    }
    let unreadable = unreadable.load(Relaxed);
    if unreadable > 0 {
        println!("{unreadable} could not be read just now (left as they are: try again).");
    }
    Ok(())
}

/// The installation registered under `alias`.
fn named_installation(ctx: &Ctx, alias: &str) -> Res<Installation> {
    let db = ctx.db.read()?;
    let entry = db.installations.get(alias).ok_or_else(|| anyhow!("no installation named \"{alias}\" (see `cactup list`)"))?;
    Ok(Installation::new(alias, &entry.path))
}

/// One build attempt's recorded compiles.
struct Recorded {
    alias: String,
    config: String,
    attempt: u32,
    events: Vec<Event>,
    /// Lines of the log that are not events this cactup can read.
    unreadable: usize,
}

impl Recorded {
    /// The attempt `attempt` of config `name`, or the newest one that
    /// recorded its compiles (other than `not`).
    fn open(installation: &Installation, name: &str, attempt: Option<u32>, not: Option<u32>) -> Res<Self> {
        let config_dir = installation.cactus_root().join("configs").join(name);
        if !config_dir.is_dir() {
            bail!("no config named \"{name}\" in installation {} (see `cactup config list`)", installation.alias);
        }
        let log = |id: u32| events_path(&BuildAttempt::attempt_dir(&config_dir, id).join("cc"));
        let attempt = match attempt {
            Some(id) if log(id).is_file() => id,
            Some(id) => bail!(
                "build attempt {id:04} of config \"{name}\" recorded no compiles: it was not built with the \
                 build-cache knob set to record, or nothing needed compiling"
            ),
            None => {
                let recorded = |id: &u32| Some(*id) != not && log(*id).is_file();
                BuildAttempt::scan(&config_dir)?.into_iter().rev().find(recorded).ok_or_else(|| {
                    anyhow!(
                        "no{} build of config \"{name}\" in installation {} recorded its compiles; build it with \
                         `cactup -K build-cache=record build {name}`",
                        if not.is_some() { " other" } else { "" },
                        installation.alias,
                    )
                })?
            }
        };
        let (events, unreadable) = event::read(&log(attempt))?;
        if events.is_empty() && unreadable > 0 {
            bail!(
                "build attempt {attempt:04} of config \"{name}\" recorded its compiles in a form this cactup cannot \
                 read (another version wrote it); build it again with `cactup -K build-cache=record build {name}`"
            );
        }
        Ok(Self { alias: installation.alias.clone(), config: name.to_owned(), attempt, events, unreadable })
    }

    /// A line for the report when part of the log could not be read, so the
    /// numbers under it are not taken for the whole build. `which` build
    /// the line stands under: "this" one, or "that" one compared against.
    fn unreadable_note(&self, which: &str) -> String {
        let note = match self.unreadable {
            0 => return String::new(),
            1 => format!("1 line of {which} build's log cannot be read and is not counted"),
            lines => format!("{lines} lines of {which} build's log cannot be read and are not counted"),
        };
        format!("  {}\n", note.yellow())
    }

    fn title(&self) -> String {
        format!("{}, build attempt {:04} (installation {})", self.config, self.attempt, self.alias)
    }
}

/// The source language of a unit, by the object's name (`a.c.o`, `b.F90.o`).
fn language(event: &Event) -> &'static str {
    let name = event.unit.as_deref().unwrap_or_default();
    let suffix = name.strip_suffix(".o").and_then(|stem| stem.rsplit_once('.')).map(|(_, suffix)| suffix);
    match suffix {
        Some("c") => "C",
        Some("cc" | "C" | "cpp" | "cxx") => "C++",
        Some("cu") => "CUDA",
        Some("F" | "f" | "F77" | "f77" | "F90" | "f90") => "Fortran",
        _ => "other",
    }
}

fn seconds(ms: u64) -> String {
    format!("{:.1} s", ms as f64 / 1000.0)
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1e6)
}

fn percent(part: u64, whole: u64) -> String {
    if whole == 0 { "-".to_owned() } else { format!("{:.0}%", 100.0 * part as f64 / whole as f64) }
}

/// Lines "count  what", most frequent first.
fn tally(counts: BTreeMap<String, u64>, indent: &str) -> String {
    let mut counts: Vec<(String, u64)> = counts.into_iter().collect();
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    counts.iter().map(|(what, count)| format!("{indent}{count:>6}  {what}\n")).collect()
}

/// What one build recorded.
fn summary(events: &[Event]) -> String {
    let total = events.len() as u64;
    let keyed: Vec<&Event> = events.iter().filter(|e| e.key.is_some()).collect();
    let sum = |field: fn(&Event) -> u64| events.iter().map(field).sum::<u64>();
    let (compile_ms, key_ms, recheck_ms) = (sum(|e| e.compile_ms), sum(|e| e.key_ms), sum(|e| e.recheck_ms));

    let mut out = format!("  compiles recorded: {total}\n");
    let mut by_language: BTreeMap<String, (u64, u64, u64)> = BTreeMap::new();
    for event in events {
        let entry = by_language.entry(language(event).to_owned()).or_default();
        entry.0 += 1;
        entry.1 += u64::from(event.key.is_some());
        entry.2 += event.compile_ms;
    }
    for (language, (count, keyed, ms)) in &by_language {
        out.push_str(&format!(
            "    {language:<8} {count:>6} compiles, {keyed:>6} keyed, {:>9} compiling ({} of the build's)\n",
            seconds(*ms),
            percent(*ms, compile_ms),
        ));
    }
    out.push_str(&format!("  keyed: {} of {total} ({})\n", keyed.len(), percent(keyed.len() as u64, total)));
    // The rest are keyed with this installation's and configuration's own
    // paths in the key: sound, and of use to later builds of this one only.
    let relocatable = keyed.iter().filter(|e| e.relocatable == Some(true)).count();
    out.push_str(&format!("    with a key another installation or configuration can share: {relocatable} of {}\n", keyed.len()));
    let mut reasons = BTreeMap::new();
    for reason in events.iter().filter_map(|e| e.not_cached.as_deref()) {
        *reasons.entry(reason.to_owned()).or_default() += 1;
    }
    if !reasons.is_empty() {
        out.push_str("  not keyed, and why:\n");
        out.push_str(&tally(reasons, "  "));
    }
    let unstable = keyed.iter().filter(|e| e.stable == Some(false)).count();
    if unstable > 0 {
        out.push_str(&format!("  key no longer held after the compile (an input changed meanwhile): {unstable}\n"));
    }
    // A compile left to the shell has no status on record at all.
    let failed = events.iter().filter(|e| e.exit.is_some_and(|code| code != 0) || e.signal.is_some()).count();
    if failed > 0 {
        out.push_str(&format!("  compiles that failed: {failed}\n"));
    }
    out.push_str(&format!(
        "  time (summed over compiles): compiling {}; keying {} ({}); checking the key again {} ({})\n",
        seconds(compile_ms),
        seconds(key_ms),
        percent(key_ms, compile_ms),
        seconds(recheck_ms),
        percent(recheck_ms, compile_ms),
    ));
    let text: u64 = events.iter().filter_map(|e| e.text_bytes).sum();
    let objects: u64 = events.iter().filter_map(|e| e.object_bytes).sum();
    out.push_str(&format!("  preprocessed text keyed: {}; objects written: {}\n", megabytes(text), megabytes(objects)));
    out.push_str(&serving(events));
    out
}

/// What a serving or auditing build did with the store (spec §18.8):
/// nothing for a build that only recorded.
fn serving(events: &[Event]) -> String {
    let tried: Vec<&Event> = events.iter().filter(|e| e.outcome.is_some()).collect();
    if tried.is_empty() {
        return String::new();
    }
    let hits = tried.iter().filter(|e| e.outcome == Some(Outcome::Hit)).count();
    let serve_ms: u64 = tried.iter().map(|e| e.serve_ms).sum();
    // In audit mode a hit is checked, not served: "found" covers both.
    let mut out = format!(
        "  found in the cache: {hits} of {} keyed ({}); looking in the store took {}\n",
        tried.len(),
        percent(hits as u64, tried.len() as u64),
        seconds(serve_ms)
    );
    let published = tried.iter().filter(|e| e.published == Some(true)).count();
    let publish_ms: u64 = tried.iter().map(|e| e.publish_ms).sum();
    out.push_str(&format!("  published: {published}, which took {}\n", seconds(publish_ms)));
    let mut notes = BTreeMap::new();
    for note in tried.iter().filter_map(|e| e.store.as_deref()) {
        *notes.entry(note.to_owned()).or_default() += 1;
    }
    if !notes.is_empty() {
        out.push_str("  what the store said:\n");
        out.push_str(&tally(notes, "  "));
    }
    let mut audits = BTreeMap::new();
    for audit in tried.iter().filter_map(|e| e.audit) {
        *audits.entry(audit.name().to_owned()).or_default() += 1;
    }
    if !audits.is_empty() {
        out.push_str("  hits checked by compiling anyway:\n");
        out.push_str(&tally(audits, "  "));
    }
    out
}

/// What of `ours` the entries `theirs` would have stored would serve.
fn comparison(ours: &[Event], theirs: &[Event], long: bool) -> String {
    // What a serving cache would have stored: a keyed compile that
    // succeeded and whose key still held afterward.
    let stored = |event: &&Event| event.key.is_some() && event.exit == Some(0) && event.stable == Some(true);
    let available: HashSet<&str> = theirs.iter().filter(stored).filter_map(|e| e.key.as_deref()).collect();
    let by_unit: HashMap<&str, &Event> = theirs.iter().filter_map(|e| Some((e.unit.as_deref()?, e))).collect();

    let keyed: Vec<&Event> = ours.iter().filter(|e| e.key.is_some()).collect();
    let served = |event: &&&Event| event.key.as_deref().is_some_and(|key| available.contains(key));
    let hits: Vec<&&Event> = keyed.iter().filter(served).collect();
    let all_ms: u64 = ours.iter().map(|e| e.compile_ms).sum();
    let hit_ms: u64 = hits.iter().map(|e| e.compile_ms).sum();

    let mut out = format!(
        "  would be served: {} of {} keyed compiles ({}), {} of {} compile time ({})\n",
        hits.len(),
        keyed.len(),
        percent(hits.len() as u64, keyed.len() as u64),
        seconds(hit_ms),
        seconds(all_ms),
        percent(hit_ms, all_ms),
    );
    let mut by_language: BTreeMap<&str, (u64, u64)> = BTreeMap::new();
    for event in &keyed {
        let entry = by_language.entry(language(event)).or_default();
        entry.0 += 1;
        entry.1 += u64::from(served(&event));
    }
    for (language, (count, hits)) in &by_language {
        out.push_str(&format!("    {language:<8} {hits:>6} of {count:>6} ({})\n", percent(*hits, *count)));
    }

    // Why the rest would miss: the part of the key that differs from the
    // same compile in the other build.
    let mut reasons = BTreeMap::new();
    let mut missed = Vec::new();
    for event in keyed.iter().filter(|e| !served(e)) {
        let unit = event.unit.as_deref();
        let reason = match unit.and_then(|unit| by_unit.get(unit)) {
            None => "not compiled in the other build".to_owned(),
            Some(other) => match (&event.parts, &other.parts) {
                (Some(mine), Some(theirs)) => {
                    let parts = [
                        ("platform", mine.platform != theirs.platform),
                        ("compiler", mine.compiler != theirs.compiler),
                        ("arguments", mine.arguments != theirs.arguments),
                        ("environment", mine.environment != theirs.environment),
                        ("preprocessed text", mine.text != theirs.text),
                        ("files read", mine.files != theirs.files),
                    ];
                    let differing: Vec<&str> = parts.iter().filter(|(_, differs)| *differs).map(|(part, _)| *part).collect();
                    match differing.as_slice() {
                        // The same key, but the other build would not have
                        // stored it.
                        [] => "not stored by the other build (it failed there, or its key did not hold)".to_owned(),
                        differing => format!("differs in: {}", differing.join(", ")),
                    }
                }
                _ => "not keyed in the other build".to_owned(),
            },
        };
        *reasons.entry(reason.clone()).or_default() += 1;
        missed.push((unit.unwrap_or("(no object)"), reason));
    }
    if !reasons.is_empty() {
        out.push_str(&format!("  would miss: {}\n", missed.len()));
        out.push_str(&tally(reasons, "  "));
    }
    if long {
        missed.sort();
        for (unit, reason) in missed {
            out.push_str(&format!("    {unit}: {reason}\n"));
        }
    }
    out
}

/// The path of an attempt's event log, for messages and tests.
#[cfg(test)]
fn log_path(config_dir: &std::path::Path, attempt: u32) -> std::path::PathBuf {
    events_path(&BuildAttempt::attempt_dir(config_dir, attempt).join("cc"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objcache::event::Audit;
    use crate::objcache::key::Parts;

    fn parts(text: &str, arguments: &str) -> Parts {
        Parts {
            platform: "p".into(),
            compiler: "c".into(),
            arguments: arguments.into(),
            environment: "e".into(),
            text: text.into(),
            files: format!("files of {text}"),
        }
    }

    fn keyed(unit: &str, parts: Parts, compile_ms: u64) -> Event {
        Event {
            compiler: "gcc".into(),
            unit: Some(unit.into()),
            exit: Some(0),
            signal: None,
            key: Some(parts.key()),
            parts: Some(parts),
            not_cached: None,
            relocatable: Some(true),
            stable: Some(true),
            text_bytes: Some(1_000_000),
            files: Some(12),
            object_bytes: Some(500_000),
            key_ms: 10,
            compile_ms,
            recheck_ms: 10,
            ..Default::default()
        }
    }

    fn not_keyed(unit: &str, why: &str, compile_ms: u64) -> Event {
        Event {
            key: None,
            parts: None,
            not_cached: Some(why.into()),
            relocatable: None,
            stable: None,
            text_bytes: None,
            files: None,
            ..keyed(unit, parts("", ""), compile_ms)
        }
    }

    #[test]
    fn tells_languages_by_object_name() {
        for (unit, expected) in [("T/a.c.o", "C"), ("T/sub/b.cc.o", "C++"), ("T/c.F90.o", "Fortran"), ("T/d.f.o", "Fortran"), ("T/e.cu.o", "CUDA"), ("T/odd.o", "other")] {
            assert_eq!(language(&not_keyed(unit, "", 0)), expected, "{unit}");
        }
    }

    #[test]
    fn summarizes_a_build() {
        let events = [
            keyed("T/a.c.o", parts("t1", "a"), 1000),
            keyed("T/b.cc.o", parts("t2", "a"), 3000),
            not_keyed("T/c.F90.o", "Fortran is not cached yet", 500),
            not_keyed("T/d.F90.o", "Fortran is not cached yet", 500),
        ];
        let text = summary(&events);
        assert!(text.contains("compiles recorded: 4\n"), "{text}");
        assert!(text.contains("keyed: 2 of 4 (50%)\n"), "{text}");
        assert!(text.contains("with a key another installation or configuration can share: 2 of 2\n"), "{text}");
        assert!(text.contains("     2  Fortran is not cached yet\n"), "{text}");
        assert!(text.contains("Fortran       2 compiles,      0 keyed,     1.0 s compiling (20% of the build's)"), "{text}");
        assert!(text.contains("compiling 5.0 s; keying 0.0 s (1%)"), "{text}");
        assert!(text.contains("preprocessed text keyed: 2.0 MB; objects written: 2.0 MB"), "{text}");
        assert!(!text.contains("no longer held") && !text.contains("failed"), "{text}");
        assert!(!text.contains("found in the cache"), "a recording build serves nothing: {text}");

        // A serving build, and an auditing one.
        let mut events = events.to_vec();
        events[0].outcome = Some(Outcome::Hit);
        events[0].audit = Some(Audit::Same);
        events[1].outcome = Some(Outcome::Miss);
        events[1].published = Some(true);
        let text = summary(&events);
        assert!(text.contains("found in the cache: 1 of 2 keyed (50%)"), "{text}");
        assert!(text.contains("published: 1,"), "{text}");
        assert!(text.contains("hits checked by compiling anyway:\n       1  same\n"), "{text}");
    }

    #[test]
    fn compares_two_builds_by_key_and_explains_misses_by_unit() {
        let ours = [
            keyed("T/same.c.o", parts("t1", "a"), 1000),
            keyed("T/moved.c.o", parts("t2", "a"), 1000),
            keyed("T/edited.c.o", parts("t3-new", "a"), 2000),
            keyed("T/flags.c.o", parts("t4", "a-new"), 4000),
            keyed("T/new.c.o", parts("t5", "a"), 1000),
            keyed("T/unstable-there.c.o", parts("t6", "a"), 1000),
            not_keyed("T/f.F90.o", "Fortran is not cached yet", 500),
        ];
        let theirs = [
            keyed("T/same.c.o", parts("t1", "a"), 1000),
            // The same compile under another name: a key is a key.
            keyed("Other/elsewhere.c.o", parts("t2", "a"), 1000),
            keyed("T/edited.c.o", parts("t3", "a"), 2000),
            keyed("T/flags.c.o", parts("t4", "a"), 4000),
            Event { stable: Some(false), ..keyed("T/unstable-there.c.o", parts("t6", "a"), 1000) },
        ];
        let text = comparison(&ours, &theirs, true);
        assert!(text.contains("would be served: 2 of 6 keyed compiles (33%), 2.0 s of 10.5 s compile time (19%)"), "{text}");
        assert!(text.contains("would miss: 4\n"), "{text}");
        for line in [
            "     1  differs in: preprocessed text, files read\n",
            "     1  differs in: arguments\n",
            "     1  not compiled in the other build\n",
            "     1  not stored by the other build (it failed there, or its key did not hold)\n",
            "    T/edited.c.o: differs in: preprocessed text, files read\n",
            "    T/new.c.o: not compiled in the other build\n",
        ] {
            assert!(text.contains(line), "{line:?} not in\n{text}");
        }
        // Without --long, no per-unit lines.
        assert!(!comparison(&ours, &theirs, false).contains("T/edited.c.o"));
    }

    #[test]
    fn finds_the_newest_recorded_attempt() {
        let tmp = tempfile::tempdir().unwrap();
        let installation = Installation::new("et", tmp.path());
        let config_dir = installation.cactus_root().join("configs/sim");
        let record = |attempt: u32, events: &[Event]| {
            let log = log_path(&config_dir, attempt);
            std::fs::create_dir_all(log.parent().unwrap()).unwrap();
            for event in events {
                event.append(&log);
            }
        };
        std::fs::create_dir_all(BuildAttempt::attempt_dir(&config_dir, 3)).unwrap();
        record(1, &[keyed("T/a.c.o", parts("t", "a"), 1)]);
        record(2, &[keyed("T/a.c.o", parts("t", "a"), 1), keyed("T/b.c.o", parts("u", "a"), 1)]);

        // Attempt 3 recorded nothing: the newest that did is 2.
        let newest = Recorded::open(&installation, "sim", None, None).unwrap();
        assert_eq!((newest.attempt, newest.events.len(), newest.unreadable), (2, 2, 0));
        assert_eq!(newest.unreadable_note("this"), "");
        assert_eq!(newest.title(), "sim, build attempt 0002 (installation et)");
        assert_eq!(Recorded::open(&installation, "sim", None, Some(2)).unwrap().attempt, 1);
        assert_eq!(Recorded::open(&installation, "sim", Some(1), None).unwrap().attempt, 1);

        let err = |attempt, not| Recorded::open(&installation, "sim", attempt, not).err().unwrap().to_string();
        assert!(err(Some(3), None).contains("build attempt 0003 of config \"sim\" recorded no compiles"));
        assert!(Recorded::open(&installation, "nope", None, None).err().unwrap().to_string().contains("no config named \"nope\""));
        std::fs::remove_file(log_path(&config_dir, 1)).unwrap();
        assert!(err(None, Some(2)).contains("no other build of config \"sim\""), "{}", err(None, Some(2)));

        // Lines this cactup cannot read are counted and said; a log with
        // nothing else is an error, not an empty build.
        let append = |attempt: u32, line: &str| {
            use std::io::Write;
            let path = log_path(&config_dir, attempt);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut log = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
            writeln!(log, "{line}").unwrap();
        };
        append(2, "{\"compiler\":\"gcc\",\"some-older-format\":true}");
        let partly = Recorded::open(&installation, "sim", Some(2), None).unwrap();
        assert_eq!((partly.events.len(), partly.unreadable), (2, 1));
        assert!(partly.unreadable_note("that").contains("1 line of that build's log cannot be read and is not counted"));
        append(3, "{\"compiler\":\"gcc\",\"some-older-format\":true}");
        assert!(err(Some(3), None).contains("in a form this cactup cannot read"), "{}", err(Some(3), None));
    }
}
