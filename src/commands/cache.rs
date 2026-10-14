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
            let build = Recorded::open(&installation, &name, attempt, None, "record")?;
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
                    Some(Recorded::open(&other_installation, &other_name, against_attempt, same_config.then_some(build.attempt), "record")?)
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
        CacheCommand::Doctor { name, attempt } => {
            let installation = Installation::resolve(ctx)?;
            let name = match name {
                Some(name) => name,
                None => installation.meta()?.active_config()?.to_owned(),
            };
            let config_dir = installation.cactus_root().join("configs").join(&name);
            if !config_dir.is_dir() {
                bail!("no config named \"{name}\" in installation {} (see `cactup config list`)", installation.alias);
            }
            // The build environment, as the newest attempt froze it.
            let newest = BuildAttempt::scan(&config_dir)?.into_iter().last();
            match newest {
                Some(id) => {
                    let attempt = BuildAttempt::open(&config_dir, id)?;
                    let conf = probe_conf(ctx, &attempt)?;
                    println!("{}", format!("{name}: the build environment of build attempt {id:04} (installation {})", installation.alias).bold());
                    let (progress, renderer) = crate::manifest::setup_prodash_if_tty();
                    let sets = crate::objcache::doctor::check_sets(&attempt, conf);
                    let names: Vec<String> = sets.iter().map(|set| format!("check the {}", set.name)).collect();
                    let layout = crate::progress::Layout::for_names(names.iter().map(String::as_str));
                    let mut findings = Vec::new();
                    for (set, name) in sets.iter().zip(&names) {
                        let line = crate::progress::Line::counting(progress.add_child(name.clone()), name.clone(), layout.clone());
                        line.phase("runs the build's environment setup and the cache's checks in it");
                        findings.extend((set.run)());
                        drop(line);
                    }
                    if let Some(renderer) = renderer {
                        renderer.shutdown_and_wait();
                    }
                    if gix::interrupt::is_triggered() {
                        bail!("interrupted");
                    }
                    print!("{}", environment_findings(&findings));
                }
                None => println!("{}", format!("{name} has not been built yet: its build environment is checked once it has").bold()),
            }
            // What the newest build that went through the cache found.
            match Recorded::open(&installation, &name, attempt, None, "serve") {
                Ok(build) => {
                    println!("\n{}", build.title().bold());
                    print!("{}{}", build.unreadable_note("this"), doctor(&build.events, build.mode));
                }
                Err(e) => println!("\n{}", format!("{e:#}").bold()),
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
        let record_bytes: u64 = machine.records.iter().map(|r| r.bytes).sum();
        match record_bytes {
            0 => println!("  {}: {} objects, {}", machine.name.bold(), machine.entries.len(), upkeep::human(bytes)),
            _ => println!(
                "  {}: {} objects, {} (and {} of records of what made them)",
                machine.name.bold(),
                machine.entries.len(),
                upkeep::human(bytes),
                upkeep::human(record_bytes)
            ),
        }
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
        if !machine.records.is_empty() {
            println!("    records of what made them (a miss reads them to say what differed): {}", machine.records.len());
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
    // The objects' bytes with their records', as the size counts them.
    let bytes: u64 = plan.entries.iter().map(|(e, _)| e.bytes).sum::<u64>() + plan.records.iter().map(|(_, _, bytes)| bytes).sum::<u64>();
    if dry_run {
        for (entry, last) in &plan.entries {
            println!("  would remove {} ({}, last used or stored {})", entry.path.display(), upkeep::human(entry.bytes), ago(now, *last));
        }
        println!(
            "Would remove {} of {} objects and {} record(s) of objects not stored or removed with them ({} in all), leaving {}; and {} file(s) left over from interrupted builds.",
            plan.entries.len(),
            scan.machines.iter().map(|m| m.entries.len()).sum::<usize>(),
            plan.records.len(),
            upkeep::human(bytes),
            upkeep::human(plan.bytes_after),
            plan.temps.len()
        );
        return Ok(());
    }
    let done = upkeep::carry_out(&plan)?;
    upkeep::stamp_size(root, plan.bytes_after)?;
    println!(
        "Removed {} objects and {} record(s) of objects no longer stored ({} in all), and {} file(s) left over from interrupted builds; the build cache holds {}.",
        done.entries,
        done.records,
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

/// `cactup cache doctor` (§18.12): why a build's compiles were not served,
/// from its event log alone.
fn doctor(events: &[Event], mode: Option<crate::objcache::Mode>) -> String {
    use crate::objcache::event::Miss;
    use crate::objcache::summary::{self, Compiled};
    let mut out = String::new();
    // By the mode the build was made in; without it, by its compiles.
    let served_by_store = mode.map_or_else(|| events.iter().any(|event| event.outcome.is_some()), |mode| mode.serves());
    if !served_by_store {
        out.push_str("  this build only recorded its compiles: nothing was served or stored (see `cactup cache report`)\n");
    } else {
        let line = summary::line(events);
        out.push_str(&format!("  {}\n", line.trim_start_matches("cactup: build cache: ").trim_end_matches(" (see cactup cache doctor)")));
    }
    let of = |kind: Compiled| -> Vec<&Event> { events.iter().filter(|event| summary::compiled(event) == Some(kind)).collect() };
    // A few objects, by name, for a reason.
    let some = |events: &[&Event]| -> String {
        let names: Vec<&str> = events.iter().filter_map(|event| event.unit.as_deref()).take(3).collect();
        match events.len().saturating_sub(names.len()) {
            0 => names.join(", "),
            more => format!("{} and {more} more", names.join(", ")),
        }
    };

    let kept = of(Compiled::KeptOut);
    if !kept.is_empty() {
        out.push_str(&format!("\n  {} ({}):\n", "kept out".bold(), kept.len()));
        // By the reason with its paths left out, with the first in full.
        let mut by: BTreeMap<String, (String, Vec<&Event>)> = BTreeMap::new();
        for event in &kept {
            let why = match (&event.not_cached, &event.store, event.stable) {
                (Some(why), _, _) => why.clone(),
                (_, _, Some(false)) => "what it read changed while it compiled".to_owned(),
                (_, Some(why), _) => why.clone(),
                _ => "the build cache could not be used".to_owned(),
            };
            by.entry(without_paths(&why)).or_insert_with(|| (why.clone(), Vec::new())).1.push(event);
        }
        let mut by: Vec<(String, (String, Vec<&Event>))> = by.into_iter().collect();
        by.sort_by(|a, b| b.1.1.len().cmp(&a.1.1.len()).then_with(|| a.0.cmp(&b.0)));
        for (why, (first, events)) in &by {
            out.push_str(&format!("    {:>5}  {why}\n           {}\n", events.len(), some(events)));
            if first != why {
                out.push_str(&format!("           the first: {first}\n"));
            }
            if let Some(hint) = kept_out_hint(why) {
                out.push_str(&format!("           {}\n", hint.green()));
            }
        }
    }

    let other = of(Compiled::OtherSources);
    if !other.is_empty() {
        out.push_str(&format!("\n  {} ({}), by what differed, most often first:\n", "with other sources".bold(), other.len()));
        // By file: a difference is `<file>: <how this key took it>`, and
        // one file taken in several ways (units that keep other
        // definitions of it) is one file all the same.
        // Each file: its misses, and how many of them took it each way.
        type Tally<'a> = (usize, HashMap<&'a str, usize>);
        let mut by: HashMap<&str, Tally> = HashMap::new();
        for event in &other {
            for differs in &event.differs {
                let (what, how) = differs.split_once(": ").unwrap_or((differs.as_str(), ""));
                let file = by.entry(what).or_default();
                file.0 += 1;
                *file.1.entry(how).or_default() += 1;
            }
        }
        let mut by: Vec<(&str, Tally)> = by.into_iter().collect();
        by.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(b.0)));
        for (what, (count, hows)) in by.iter().take(20) {
            let mut hows: Vec<(&str, usize)> = hows.iter().map(|(how, n)| (*how, *n)).collect();
            hows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
            match hows.as_slice() {
                [("", _)] => out.push_str(&format!("    {count:>5}  {what}\n")),
                [(how, _)] => out.push_str(&format!("    {count:>5}  {what}: {how}\n")),
                _ => {
                    out.push_str(&format!("    {count:>5}  {what}\n"));
                    for (how, n) in hows.iter().take(3) {
                        out.push_str(&format!("           {n} {how}\n"));
                    }
                    if hows.len() > 3 {
                        out.push_str(&format!("           and {} other ways\n", hows.len() - 3));
                    }
                }
            }
        }
        if by.len() > 20 {
            out.push_str(&format!("           and {} more\n", by.len() - 20));
        }
        let unlisted: u64 = other.iter().map(|event| event.differs_more).sum();
        if unlisted > 0 {
            out.push_str(&format!("           (and {unlisted} differences the compiles did not list: each lists at most {})\n", crate::objcache::event::DIFFERS_KEPT));
        }
    }

    let otherwise = of(Compiled::BuiltOtherwise);
    if !otherwise.is_empty() {
        out.push_str(&format!("\n  {} ({}), by what differed from the closest:\n", "built otherwise".bold(), otherwise.len()));
        let mut by: BTreeMap<String, Vec<&Event>> = BTreeMap::new();
        for event in &otherwise {
            by.entry(event.differs.join(", ")).or_default().push(event);
        }
        for (differs, events) in &by {
            out.push_str(&format!("    {:>5}  {differs}\n           {}\n", events.len(), some(events)));
        }
    }
    let new = events.iter().filter(|event| event.miss == Some(Miss::New) && summary::compiled(event) == Some(Compiled::New)).count();
    if new > 0 {
        out.push_str(&format!("\n  {} ({}): the cache had no record of an object of that name (one stored by an older cactup has none)\n", "new".bold(), new));
    }

    let unrecorded = events
        .iter()
        .filter(|event| event.store.as_deref().is_some_and(|why| why.contains(crate::objcache::wrapper::UNIT_RECORD_NOT_WRITTEN)))
        .count();
    if unrecorded > 0 {
        let (count, it, were) = match unrecorded {
            1 => ("1 object was".to_owned(), "it", "its miss"),
            n => (format!("{n} objects were"), "them", "their misses"),
        };
        out.push_str(&format!(
            "\n  {count} stored without the record of what made {it} (the build could not write {it}: see its event log), so later builds may call {were} new\n"
        ));
    }

    let mut unnarrowed: BTreeMap<&str, usize> = BTreeMap::new();
    for event in events {
        if let Some(why) = &event.unnarrowed {
            *unnarrowed.entry(why.as_str()).or_default() += 1;
        }
    }
    if !unnarrowed.is_empty() {
        out.push_str(&format!(
            "\n  {} (every file compared whole, definitions the unit does not use too):\n",
            "compiles of which no file was narrowed".bold()
        ));
        let mut unnarrowed: Vec<(&str, usize)> = unnarrowed.into_iter().collect();
        unnarrowed.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        for (why, count) in unnarrowed {
            out.push_str(&format!("    {count:>5}  {why}\n"));
        }
    }
    out
}

/// A reason with the paths in it left out (as `<path>`), so that one cause
/// is one line however many files it struck: an entry that cannot be read,
/// a temporary file that cannot be made.
fn without_paths(why: &str) -> String {
    why.split(' ')
        .map(|word| match word.trim_start_matches(['(', '"', '\'']).starts_with('/') {
            true => {
                // What follows the path in the same word (`:`, `)`) stays.
                let end = word.find([':', ')', ',', ';']).unwrap_or(word.len());
                let start = word.find('/').unwrap_or(0);
                format!("{}<path>{}", &word[..start], &word[end..])
            }
            false => word.to_owned(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The cache's settings a build of `attempt` would freeze: the attempt's
/// own where it went through the cache, else the knobs now (§18.2). The
/// checks key a probe under them (in record mode: nothing is stored).
fn probe_conf(ctx: &Ctx, attempt: &BuildAttempt) -> Res<crate::objcache::BuildConf> {
    use crate::objcache::{conf_path, BuildConf, Mode};
    if let Ok(conf) = BuildConf::load(&conf_path(&attempt.dir.join("cc"))) {
        return Ok(conf);
    }
    let db = ctx.db.read()?;
    Ok(BuildConf {
        mode: Mode::Record,
        cactup: std::env::current_exe().unwrap_or_default(),
        config_dir: attempt.meta.config_dir.clone(),
        cactus_root: attempt.meta.cactus_root.clone(),
        machine: attempt.meta.machine.clone(),
        universe: None,
        build_env_digest: String::new(),
        store: PathBuf::from("/nonexistent-cactup-store"),
        relocate: crate::objcache::relocate_from_db(&db),
        exact_warnings: crate::objcache::exact_warnings_from_db(&db),
        size_limit: None,
    })
}

/// The findings of the build environment's checks, by how much they
/// matter, each with where it comes from and what to change.
fn environment_findings(findings: &[crate::objcache::doctor::Finding]) -> String {
    use crate::objcache::doctor::Severity;
    let mut out = String::new();
    // All clear only where everything was checked.
    if !findings.iter().any(|finding| matches!(finding.severity, Severity::KeepsOut | Severity::Unchecked)) {
        out.push_str(&format!("  {}\n", "nothing in it keeps the build cache out of a compile".green()));
    }
    for (severity, heading) in [
        (Severity::Unchecked, "not checked"),
        (Severity::KeepsOut, "keeps the build cache out"),
        (Severity::Costs, "costs objects shared, or time"),
        (Severity::Note, "worth knowing"),
    ] {
        let these: Vec<_> = findings.iter().filter(|finding| finding.severity == severity).collect();
        if these.is_empty() {
            continue;
        }
        out.push_str(&format!("\n  {} ({}):\n", heading.bold(), these.len()));
        for finding in these {
            out.push_str(&format!("    {}\n      from: {}\n", finding.what, finding.from));
            if let Some(fix) = &finding.fix {
                out.push_str(&format!("      {}\n", fix.green()));
            }
        }
    }
    out
}

/// What would let compiles kept out for `why` in, where cactup knows.
fn kept_out_hint(why: &str) -> Option<&'static str> {
    crate::objcache::doctor::hint(why)
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
    /// The cache's mode the build was made in, as frozen for it.
    mode: Option<crate::objcache::Mode>,
    events: Vec<Event>,
    /// Lines of the log that are not events this cactup can read.
    unreadable: usize,
}

impl Recorded {
    /// The attempt `attempt` of config `name`, or the newest one that
    /// recorded its compiles (other than `not`).
    /// `mode` is the build-cache knob's value a build to look at would have
    /// been made with, for the message when there is none.
    fn open(installation: &Installation, name: &str, attempt: Option<u32>, not: Option<u32>, mode: &str) -> Res<Self> {
        let config_dir = installation.cactus_root().join("configs").join(name);
        if !config_dir.is_dir() {
            bail!("no config named \"{name}\" in installation {} (see `cactup config list`)", installation.alias);
        }
        let log = |id: u32| events_path(&BuildAttempt::attempt_dir(&config_dir, id).join("cc"));
        let attempt = match attempt {
            Some(id) if log(id).is_file() => id,
            Some(id) => bail!(
                "build attempt {id:04} of config \"{name}\" logged no compiles: it was not built with the \
                 build-cache knob on, or nothing needed compiling"
            ),
            None => {
                let recorded = |id: &u32| Some(*id) != not && log(*id).is_file();
                BuildAttempt::scan(&config_dir)?.into_iter().rev().find(recorded).ok_or_else(|| {
                    anyhow!(
                        "no{} build of config \"{name}\" in installation {} logged its compiles; build it with \
                         `cactup -K build-cache={mode} build {name}`",
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
                 read (another version wrote it); build it again with `cactup -K build-cache={mode} build {name}`"
            );
        }
        let cc_dir = BuildAttempt::attempt_dir(&config_dir, attempt).join("cc");
        let mode = crate::objcache::BuildConf::load(&crate::objcache::conf_path(&cc_dir)).ok().map(|conf| conf.mode);
        Ok(Self { alias: installation.alias.clone(), config: name.to_owned(), attempt, mode, events, unreadable })
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

/// The source language of a unit, by the object's name (`a.c.o`, `b.F90.o`);
/// CUDA by the compiler too (`nvcc` compiles CarpetX's `.cxx` files as
/// CUDA, `-x cu`).
fn language(event: &Event) -> &'static str {
    if event.compiler.rsplit('/').next().is_some_and(|name| name == "nvcc") {
        return "CUDA";
    }
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

    #[test]
    fn the_doctor_says_why_compiles_were_not_served() {
        use crate::objcache::event::Miss;
        colored::control::set_override(false);
        let miss = |unit: &str, kind: Miss, differs: &[&str]| Event {
            unit: Some(unit.into()),
            key: Some("k".into()),
            outcome: Some(Outcome::Miss),
            exit: Some(0),
            stable: Some(true),
            published: Some(true),
            miss: Some(kind),
            differs: differs.iter().map(|d| d.to_string()).collect(),
            ..Default::default()
        };
        let header = "/cactup-root/configs/@config/bindings/include/cctk_DefineThorn.h: whole: the compiled text has code of it";
        let events = vec![
            Event { unit: Some("A/a.c.o".into()), key: Some("k".into()), outcome: Some(Outcome::Hit), exit: Some(0), ..Default::default() },
            miss("A/b.c.o", Miss::New, &[]),
            miss("A/c.c.o", Miss::OtherSources, &[header]),
            Event { differs_more: 3, ..miss("A/d.c.o", Miss::OtherSources, &[header, "the preprocessed text"]) },
            miss("A/e.c.o", Miss::BuiltOtherwise, &["the flags"]),
            Event { unit: Some("B/f.cu.o".into()), not_cached: Some("nvcc compiles without --objdir-as-tempdir are not the same twice".into()), exit: Some(0), ..Default::default() },
            Event { unit: Some("C/g.F90.o".into()), unnarrowed: Some("Fortran (its files are keyed whole)".into()), ..miss("C/g.F90.o", Miss::New, &[]) },
        ];
        let said = doctor(&events, Some(crate::objcache::Mode::Serve));
        assert!(said.starts_with("  1 of 7 objects reused; 6 compiled: 2 new, 2 with other sources, 1 built otherwise, 1 kept out\n"), "{said}");
        assert!(said.contains("kept out (1):\n        1  nvcc compiles without --objdir-as-tempdir are not the same twice\n           B/f.cu.o\n           give nvcc --objdir-as-tempdir"), "{said}");
        assert!(said.contains(&format!("        2  {header}\n        1  the preprocessed text\n           (and 3 differences the compiles did not list")), "{said}");
        assert!(said.contains("built otherwise (1), by what differed from the closest:\n        1  the flags\n           A/e.c.o\n"), "{said}");
        assert!(said.contains("        1  Fortran (its files are keyed whole)\n"), "{said}");

        // One header taken in two ways by units that keep other
        // definitions of it is one file, ranked by all its misses.
        let big = |how: &str| format!("/cactup-root/big.h: narrowed: a line the key keeps differs ({how})");
        let split = [
            miss("B/a.c.o", Miss::OtherSources, &[&big("U1")]),
            miss("B/b.c.o", Miss::OtherSources, &[&big("U2")]),
            miss("B/c.c.o", Miss::OtherSources, &[&big("U1")]),
            miss("B/d.c.o", Miss::OtherSources, &["/cactup-root/small.h: whole: the compiled text has code of it"]),
            miss("B/e.c.o", Miss::OtherSources, &["/cactup-root/small.h: whole: the compiled text has code of it"]),
        ];
        let said = doctor(&split, Some(crate::objcache::Mode::Serve));
        assert!(
            said.contains(&format!(
                "        3  /cactup-root/big.h\n           2 {}\n           1 {}\n        2  /cactup-root/small.h: whole: the compiled text has code of it\n",
                big("U1").trim_start_matches("/cactup-root/big.h: "),
                big("U2").trim_start_matches("/cactup-root/big.h: ")
            )),
            "{said}"
        );

        // A serving build whose every compile was kept out is no build
        // that only recorded; one cause with a path in it is one line.
        let unreadable = |unit: &str, path: &str| Event {
            unit: Some(unit.into()),
            key: Some("k".into()),
            outcome: Some(Outcome::Miss),
            exit: Some(0),
            stable: Some(true),
            miss: Some(Miss::Unusable),
            store: Some(format!("the entry could not be read: {path}: Permission denied")),
            ..Default::default()
        };
        let events = [unreadable("A/a.c.o", "/s/v4/m/0a/0a12"), unreadable("A/b.c.o", "/s/v4/m/1b/1b34")];
        let said = doctor(&events, Some(crate::objcache::Mode::Serve));
        assert!(!said.contains("only recorded"), "{said}");
        assert!(
            said.contains("        2  the entry could not be read: <path>: Permission denied\n           A/a.c.o, A/b.c.o\n           the first: the entry could not be read: /s/v4/m/0a/0a12: Permission denied\n"),
            "{said}"
        );
        assert!(doctor(&events[..1], Some(crate::objcache::Mode::Record)).contains("only recorded"));
        assert_eq!(without_paths("not published: Failed to create a temporary file in /s/v4/m/0a (Permission denied)"), "not published: Failed to create a temporary file in <path> (Permission denied)");
    }

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
        let newest = Recorded::open(&installation, "sim", None, None, "record").unwrap();
        assert_eq!((newest.attempt, newest.events.len(), newest.unreadable), (2, 2, 0));
        assert_eq!(newest.unreadable_note("this"), "");
        assert_eq!(newest.title(), "sim, build attempt 0002 (installation et)");
        assert_eq!(Recorded::open(&installation, "sim", None, Some(2), "record").unwrap().attempt, 1);
        assert_eq!(Recorded::open(&installation, "sim", Some(1), None, "record").unwrap().attempt, 1);

        let err = |attempt, not| Recorded::open(&installation, "sim", attempt, not, "serve").err().unwrap().to_string();
        assert!(err(Some(3), None).contains("build attempt 0003 of config \"sim\" logged no compiles"));
        assert!(Recorded::open(&installation, "nope", None, None, "record").err().unwrap().to_string().contains("no config named \"nope\""));
        std::fs::remove_file(log_path(&config_dir, 1)).unwrap();
        assert!(err(None, Some(2)).contains("no other build of config \"sim\""), "{}", err(None, Some(2)));
        assert!(err(None, Some(2)).contains("-K build-cache=serve"), "{}", err(None, Some(2)));

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
        let partly = Recorded::open(&installation, "sim", Some(2), None, "record").unwrap();
        assert_eq!((partly.events.len(), partly.unreadable), (2, 1));
        assert!(partly.unreadable_note("that").contains("1 line of that build's log cannot be read and is not counted"));
        append(3, "{\"compiler\":\"gcc\",\"some-older-format\":true}");
        assert!(err(Some(3), None).contains("in a form this cactup cannot read"), "{}", err(Some(3), None));
    }
}
