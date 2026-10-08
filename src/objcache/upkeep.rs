//! Looking after the store (§18.9): when each entry was last used, what is
//! in the store, what `cache gc` removes, and the size notice.
//!
//! Nothing here runs on its own but the use log a serving build writes and
//! the size notice: every removal is a command someone typed.
//!
//! **Last use.** An entry is never changed after its link (§18.7), so its
//! uses are kept beside it: each serving build writes one new file into
//! `used/` listing the keys it found, whose modification time — the
//! fileserver's clock — is when it found them. `gc` folds the logs it read
//! into one that carries a time on each line, and removes them.

use super::store::{Store, FORMAT};
use crate::Res;
use anyhow::{bail, Context};
use std::collections::HashMap;
use std::fs::{self, Metadata};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The directory of a machine's use logs.
fn used_dir(machine_dir: &Path) -> PathBuf {
    machine_dir.join("used")
}

/// Record that a build found `keys` in the store (§18.9): one new file, its
/// modification time the fileserver's clock now. Nothing for no keys.
pub fn log_use(store: &Store, keys: &[String]) -> Res<()> {
    if keys.is_empty() {
        return Ok(());
    }
    let dir = used_dir(store.dir());
    fs::create_dir_all(&dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    let mut temp = readable_temp(&dir)?;
    let mut text = keys.join("\n");
    text.push('\n');
    temp.write_all(text.as_bytes()).context("Failed to write a use log")?;
    let name = dir.join(format!("{:016x}{:016x}.keys", fastrand::u64(..), fastrand::u64(..)));
    temp.persist_noclobber(&name).map_err(|e| e.error).with_context(|| format!("Failed to write {}", name.display()))?;
    Ok(())
}

/// A temporary file in `dir` for a file of the store that everyone who
/// can read the store must be able to read (a use log, the size stamp):
/// `0666` less the umask, as entries and objects are, not `tempfile`'s
/// `0600`. Another user's `gc` that could not read a use log would not know
/// what it says is in use.
fn readable_temp(dir: &Path) -> Res<tempfile::NamedTempFile> {
    use std::os::unix::fs::PermissionsExt;
    tempfile::Builder::new()
        .prefix(".tmp-use-")
        .permissions(fs::Permissions::from_mode(0o666))
        .tempfile_in(dir)
        .with_context(|| format!("Failed to create a temporary file in {}", dir.display()))
}

/// One entry, as the walk found it.
#[derive(Debug, Clone)]
pub struct Entry {
    pub key: String,
    pub path: PathBuf,
    pub bytes: u64,
    /// When it was published: its modification time.
    pub published: SystemTime,
    dev: u64,
    ino: u64,
}

impl Entry {
    /// Is the file at this entry's name still the one the walk found? Not
    /// by inode alone: ext4 gives a new file a freed inode number at once,
    /// and an entry republished meanwhile is new. An entry is never changed
    /// after its link, so its size and modification time tell.
    fn still_there(&self) -> bool {
        fs::symlink_metadata(&self.path).is_ok_and(|now| {
            (now.dev(), now.ino(), now.len(), modified(&now)) == (self.dev, self.ino, self.bytes, self.published)
        })
    }
}

/// A file the walk found that is not an entry: a temporary file a publish
/// cut short left, or a use log.
#[derive(Debug, Clone)]
pub struct Other {
    pub path: PathBuf,
    pub modified: SystemTime,
}

/// What a use log says: keys, each with when it was used.
#[derive(Debug, Clone)]
pub struct Log {
    pub path: PathBuf,
    pub uses: Vec<(String, SystemTime)>,
}

/// One machine's part of the store, as the walk found it.
#[derive(Debug, Default)]
pub struct Machine {
    pub name: String,
    pub dir: PathBuf,
    pub entries: Vec<Entry>,
    pub temps: Vec<Other>,
    pub logs: Vec<Log>,
}

impl Machine {
    /// When each key was last used, by the logs (a key no log names is not
    /// in here: its publish time is all there is).
    pub fn last_uses(&self) -> HashMap<&str, SystemTime> {
        let mut last: HashMap<&str, SystemTime> = HashMap::new();
        for (key, when) in self.logs.iter().flat_map(|log| log.uses.iter()) {
            let at = last.entry(key.as_str()).or_insert(*when);
            *at = (*at).max(*when);
        }
        last
    }
}

/// The whole store of this format, and what else is beside it.
#[derive(Debug, Default)]
pub struct Scan {
    pub machines: Vec<Machine>,
    /// Directories of other entry formats (another version of cactup's),
    /// by path. Not walked: nothing here reads or removes them.
    pub other_formats: Vec<PathBuf>,
    /// Use logs that could not be looked at or read, and why. What they
    /// record as in use is not known: `gc` must not run.
    pub unreadable_logs: Vec<(PathBuf, String)>,
}

impl Scan {
    pub fn bytes(&self) -> u64 {
        self.machines.iter().flat_map(|m| m.entries.iter()).map(|e| e.bytes).sum()
    }
}

/// One directory the walk reads: a machine's entry directory (`ab/`), or
/// its `used/`.
struct Piece {
    machine: usize,
    dir: PathBuf,
    used: bool,
}

/// What one piece held.
#[derive(Default)]
struct Found {
    entries: Vec<Entry>,
    temps: Vec<Other>,
    logs: Vec<Log>,
    unreadable_logs: Vec<(PathBuf, String)>,
}

/// Is `name` a key (64 lowercase hex digits)?
fn is_key(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn modified(meta: &Metadata) -> SystemTime {
    meta.modified().unwrap_or(UNIX_EPOCH)
}

/// Read one piece of the store.
fn read_piece(piece: &Piece) -> Res<Found> {
    let mut found = Found::default();
    let listing = match fs::read_dir(&piece.dir) {
        Ok(listing) => listing,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(found),
        Err(e) => return Err(e).with_context(|| format!("Failed to read {}", piece.dir.display())),
    };
    for item in listing {
        let item = item.with_context(|| format!("Failed to read {}", piece.dir.display()))?;
        let name = item.file_name().to_string_lossy().into_owned();
        let path = item.path();
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            // A use log that cannot be looked at is not known to say
            // nothing; an entry that cannot is no entry to remove.
            Err(e) if piece.used && !name.starts_with(".tmp-") => {
                found.unreadable_logs.push((path, e.to_string()));
                continue;
            }
            Err(_) => continue,
        };
        if !meta.is_file() {
            continue;
        }
        if name.starts_with(".tmp-") {
            found.temps.push(Other { path, modified: modified(&meta) });
        } else if piece.used {
            let kind = name.rsplit_once('.').map(|(_, kind)| kind);
            if !matches!(kind, Some("keys" | "times")) {
                continue;
            }
            // A log that cannot be read is kept apart: what it says is in use
            // is not known, and must not be taken for nothing (`gc` will not
            // run while there is one).
            let text = match fs::read_to_string(&path) {
                Ok(text) => text,
                Err(e) => {
                    found.unreadable_logs.push((path, e.to_string()));
                    continue;
                }
            };
            let uses = match kind {
                // Every key in it, at the time it was written.
                Some("keys") => text
                    .lines()
                    .filter(|key| is_key(key))
                    .map(|key| (key.to_owned(), modified(&meta)))
                    .collect(),
                // `<key> <seconds>` on each line, as `gc` folded them.
                _ => text
                    .lines()
                    .filter_map(|line| {
                        let (key, seconds) = line.split_once(' ')?;
                        let seconds: u64 = seconds.parse().ok()?;
                        is_key(key).then(|| (key.to_owned(), UNIX_EPOCH + Duration::from_secs(seconds)))
                    })
                    .collect(),
            };
            found.logs.push(Log { path, uses });
        } else if is_key(&name) {
            found.entries.push(Entry {
                key: name,
                path,
                bytes: meta.len(),
                published: modified(&meta),
                dev: meta.dev(),
                ino: meta.ino(),
            });
        }
    }
    Ok(found)
}

/// Walk the store under `root`: every machine's entries, temporary files and
/// use logs, and what directories of other formats there are. The entry
/// directories are read in parallel (`par::parallel_map`, which stops on
/// Ctrl-C), with a progress line counting them.
pub fn scan(root: &Path) -> Res<Scan> {
    let format_dir = root.join(format!("v{FORMAT}"));
    // Every listing is complete or an error: a directory that drops out of
    // the walk would look empty, and its entries unused.
    let list = |dir: &Path| -> Res<Vec<fs::DirEntry>> {
        match fs::read_dir(dir) {
            Ok(listing) => listing.collect::<Result<Vec<_>, _>>().with_context(|| format!("Failed to read {}", dir.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e).with_context(|| format!("Failed to read {}", dir.display())),
        }
    };
    let mut scan = Scan::default();
    for item in list(root)? {
        let name = item.file_name().to_string_lossy().into_owned();
        let is_format = name.strip_prefix('v').is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
        if is_format && name != format!("v{FORMAT}") && item.path().is_dir() {
            scan.other_formats.push(item.path());
        }
    }
    let mut pieces = Vec::new();
    for item in list(&format_dir)? {
        let name = item.file_name().to_string_lossy().into_owned();
        let dir = item.path();
        if !dir.is_dir() || name.starts_with('.') {
            continue;
        }
        let machine = scan.machines.len();
        for sub in list(&dir)? {
            let sub_name = sub.file_name().to_string_lossy().into_owned();
            let prefix = sub_name.len() == 2 && sub_name.bytes().all(|b| b.is_ascii_hexdigit());
            if prefix || sub_name == "used" {
                pieces.push(Piece { machine, dir: sub.path(), used: sub_name == "used" });
            }
        }
        scan.machines.push(Machine { name, dir, ..Default::default() });
    }

    let (progress, renderer) = crate::manifest::setup_prodash_if_tty();
    let reading = progress.add_child("read the build cache");
    reading.init(Some(pieces.len()), Some(prodash::unit::label("directories")));
    let reading = std::sync::Mutex::new(reading);
    let found = crate::par::parallel_map(&pieces, |piece| {
        let found = read_piece(piece);
        reading.lock().expect("progress poisoned").inc();
        found
    });
    drop(reading);
    if let Some(renderer) = renderer {
        renderer.shutdown_and_wait();
    }
    for (piece, found) in pieces.iter().zip(found?) {
        let found = found?;
        let machine = &mut scan.machines[piece.machine];
        machine.entries.extend(found.entries);
        machine.temps.extend(found.temps);
        machine.logs.extend(found.logs);
        scan.unreadable_logs.extend(found.unreadable_logs);
    }
    Ok(scan)
}

/// What `cache gc` is to do (§18.9), worked out from a walk and the
/// fileserver's "now", before anything is removed.
#[derive(Debug, Default)]
pub struct Plan {
    /// Entries to remove, with when each was last used or published.
    pub entries: Vec<(Entry, SystemTime)>,
    /// Temporary files a day old.
    pub temps: Vec<Other>,
    /// Per machine: the logs to fold, and the uses to keep (the last of
    /// each key that stays).
    pub folds: Vec<(PathBuf, Vec<PathBuf>, Vec<(String, SystemTime)>)>,
    /// The bytes the store will hold after.
    pub bytes_after: u64,
}

/// Work out what to remove: the entries not used or published since `now -
/// unused_for` (with an age); then, with `to_size`, the least recently used
/// of the rest until no more than `to_size` bytes remain.
pub fn plan(scan: &Scan, now: SystemTime, unused_for: Option<Duration>, to_size: Option<u64>) -> Plan {
    // Without an age, nothing is old: only the size decides.
    let cutoff = unused_for.map_or(UNIX_EPOCH, |unused_for| now.checked_sub(unused_for).unwrap_or(UNIX_EPOCH));
    let mut plan = Plan::default();
    let mut kept: Vec<(&Entry, SystemTime, usize)> = Vec::new();
    for (index, machine) in scan.machines.iter().enumerate() {
        let last_uses = machine.last_uses();
        for entry in &machine.entries {
            let last = last_uses.get(entry.key.as_str()).map_or(entry.published, |used| (*used).max(entry.published));
            match last < cutoff {
                true => plan.entries.push((entry.clone(), last)),
                false => kept.push((entry, last, index)),
            }
        }
        let a_day_ago = now.checked_sub(Duration::from_secs(24 * 3600)).unwrap_or(UNIX_EPOCH);
        plan.temps.extend(machine.temps.iter().filter(|temp| temp.modified < a_day_ago).cloned());
    }
    let mut bytes: u64 = kept.iter().map(|(entry, _, _)| entry.bytes).sum();
    if let Some(to_size) = to_size {
        // Least recently used first.
        kept.sort_by_key(|(_, last, _)| *last);
        let mut over = kept.into_iter().peekable();
        while bytes > to_size
            && let Some((entry, last, _)) = over.next()
        {
            bytes -= entry.bytes;
            plan.entries.push((entry.clone(), last));
        }
        kept = over.collect();
    }
    plan.bytes_after = bytes;
    // Fold every machine's logs into one with the last use of each key that
    // stays (and that a log knows a use of).
    for (index, machine) in scan.machines.iter().enumerate() {
        if machine.logs.is_empty() {
            continue;
        }
        let last_uses = machine.last_uses();
        let mut uses: Vec<(String, SystemTime)> = kept
            .iter()
            .filter(|(_, _, of)| *of == index)
            .filter_map(|(entry, _, _)| last_uses.get(entry.key.as_str()).map(|when| (entry.key.clone(), *when)))
            .collect();
        uses.sort();
        let logs = machine.logs.iter().map(|log| log.path.clone()).collect();
        plan.folds.push((used_dir(&machine.dir), logs, uses));
    }
    plan
}

/// What carrying out a plan did.
#[derive(Debug, Default)]
pub struct Done {
    pub entries: u64,
    pub bytes: u64,
    pub temps: u64,
}

/// Carry out `plan`. An entry is removed only if its name still leads to
/// the file the walk found (another build may have removed it and published
/// it anew). Stops on Ctrl-C, with "interrupted", having said nothing it did
/// not do: what was removed until then is in the error.
pub fn carry_out(plan: &Plan) -> Res<Done> {
    let mut done = Done::default();
    let (progress, renderer) = crate::manifest::setup_prodash_if_tty();
    let removing = progress.add_child("remove from the build cache");
    removing.init(Some(plan.entries.len() + plan.temps.len()), Some(prodash::unit::label("files")));
    let result = (|| -> Res<()> {
        for (entry, _) in &plan.entries {
            if gix::interrupt::is_triggered() {
                bail!("interrupted, after removing {} objects ({})", done.entries, human(done.bytes));
            }
            if entry.still_there() && fs::remove_file(&entry.path).is_ok() {
                done.entries += 1;
                done.bytes += entry.bytes;
            }
            removing.inc();
        }
        for temp in &plan.temps {
            if gix::interrupt::is_triggered() {
                bail!("interrupted, after removing {} objects ({})", done.entries, human(done.bytes));
            }
            if fs::remove_file(&temp.path).is_ok() {
                done.temps += 1;
            }
            removing.inc();
        }
        // The logs, folded: the new one in place before the old ones go.
        for (dir, logs, uses) in &plan.folds {
            let mut text = String::new();
            for (key, when) in uses {
                let seconds = when.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
                text.push_str(&format!("{key} {seconds}\n"));
            }
            if !text.is_empty() {
                let mut temp = readable_temp(dir)?;
                temp.write_all(text.as_bytes()).context("Failed to write the folded use log")?;
                let name = dir.join(format!("{:016x}{:016x}.times", fastrand::u64(..), fastrand::u64(..)));
                temp.persist_noclobber(&name).map_err(|e| e.error).with_context(|| format!("Failed to write {}", name.display()))?;
            }
            for log in logs {
                let _ = fs::remove_file(log);
            }
        }
        Ok(())
    })();
    drop(removing);
    if let Some(renderer) = renderer {
        renderer.shutdown_and_wait();
    }
    result.map(|()| done)
}

/// Read a size: bytes, or a number with `K`, `M`, `G` or `T` (powers of
/// 1024), as `200G`.
pub fn parse_size(text: &str) -> Res<u64> {
    let text = text.trim();
    let (number, unit) = match text.find(|c: char| !c.is_ascii_digit() && c != '.') {
        Some(at) => text.split_at(at),
        None => (text, ""),
    };
    let factor: u64 = match unit.trim().to_ascii_uppercase().trim_end_matches('B') {
        "" => 1,
        "K" => 1 << 10,
        "M" => 1 << 20,
        "G" => 1 << 30,
        "T" => 1 << 40,
        _ => bail!("\"{text}\" is not a size (a number, with K, M, G or T)"),
    };
    let number: f64 = number.parse().map_err(|_| anyhow::anyhow!("\"{text}\" is not a size (a number, with K, M, G or T)"))?;
    Ok((number * factor as f64) as u64)
}

/// Read an age: a number with `h`, `d`, `w`, `m` (30 days) or `y` (365
/// days), as `30d`.
pub fn parse_age(text: &str) -> Res<Duration> {
    let text = text.trim();
    let bad = || anyhow::anyhow!("\"{text}\" is not an age (a number with h, d, w, m or y, as 30d)");
    let (number, unit) = text.split_at(text.find(|c: char| !c.is_ascii_digit()).ok_or_else(bad)?);
    let number: u64 = number.parse().map_err(|_| bad())?;
    let hours: u64 = match unit {
        "h" => 1,
        "d" => 24,
        "w" => 24 * 7,
        "m" => 24 * 30,
        "y" => 24 * 365,
        _ => return Err(bad()),
    };
    let seconds = number.checked_mul(hours).and_then(|hours| hours.checked_mul(3600)).ok_or_else(bad)?;
    Ok(Duration::from_secs(seconds))
}

/// A size as people read it: `1.5 GB` (powers of 1024).
pub fn human(bytes: u64) -> String {
    let units = ["bytes", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < units.len() {
        value /= 1024.0;
        unit += 1;
    }
    match unit {
        0 => format!("{bytes} bytes"),
        _ => format!("{value:.1} {}", units[unit]),
    }
}

/// The store's measured size (§18.9), kept in `<root>/v<FORMAT>/size`.
fn size_stamp(root: &Path) -> PathBuf {
    root.join(format!("v{FORMAT}")).join("size")
}

/// Keep `bytes` as the store's size. Written whole and moved into place, so
/// its modification time is the fileserver's clock.
pub fn stamp_size(root: &Path, bytes: u64) -> Res<()> {
    let stamp = size_stamp(root);
    let dir = stamp.parent().expect("the stamp is in a directory");
    fs::create_dir_all(dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    let mut temp = readable_temp(dir)?;
    temp.write_all(format!("{bytes}\n").as_bytes()).context("Failed to write the size stamp")?;
    temp.persist(&stamp).map_err(|e| e.error).with_context(|| format!("Failed to write {}", stamp.display()))?;
    Ok(())
}

/// The store's size as a build sees it (§18.9), without walking it: the
/// size `cache stats` or `cache gc` last measured, plus what builds have
/// published since, each adding its own and keeping the sum. Builds that
/// add at the same moment can lose each other's additions, so it is an
/// estimate, low if anything; the next `stats` or `gc` measures again.
///
/// `None` when no size has been measured yet: counting from nothing would
/// make a large store look small.
pub fn add_to_size(root: &Path, published: u64) -> Res<Option<u64>> {
    let stamp = size_stamp(root);
    let Some(known) = fs::read_to_string(&stamp).ok().and_then(|text| text.trim().parse::<u64>().ok()) else {
        return Ok(None);
    };
    let bytes = known + published;
    if published > 0 {
        stamp_size(root, bytes)?;
    }
    Ok(Some(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(key: char, bytes: u64, published: u64) -> Entry {
        Entry {
            key: key.to_string().repeat(64),
            path: PathBuf::from(format!("/s/{key}")),
            bytes,
            published: UNIX_EPOCH + Duration::from_secs(published),
            dev: 1,
            ino: 1,
        }
    }

    const DAY: u64 = 24 * 3600;

    /// A store of one machine: entries `a` (published on day 1, used on day
    /// 9 by a log), `b` (published on day 2, never used since), `c`
    /// (published on day 8), each of 100 bytes.
    fn store() -> Scan {
        let uses = vec![("a".repeat(64), UNIX_EPOCH + Duration::from_secs(9 * DAY))];
        Scan {
            machines: vec![Machine {
                name: "m".into(),
                dir: PathBuf::from("/s/v1/m"),
                entries: vec![entry('a', 100, DAY), entry('b', 100, 2 * DAY), entry('c', 100, 8 * DAY)],
                temps: vec![Other { path: PathBuf::from("/s/.tmp-x"), modified: UNIX_EPOCH + Duration::from_secs(5 * DAY) }],
                logs: vec![Log { path: PathBuf::from("/s/v1/m/used/x.keys"), uses }],
            }],
            other_formats: Vec::new(),
            unreadable_logs: Vec::new(),
        }
    }

    fn keys(plan: &Plan) -> Vec<char> {
        plan.entries.iter().map(|(entry, _)| entry.key.chars().next().unwrap()).collect()
    }

    #[test]
    fn gc_removes_what_was_not_used_or_published_lately() {
        let now = UNIX_EPOCH + Duration::from_secs(10 * DAY);
        // Unused for 5 days: `b` (day 2); `a` was used on day 9, `c`
        // published on day 8.
        let plan = plan(&store(), now, Some(Duration::from_secs(5 * DAY)), None);
        assert_eq!(keys(&plan), ['b']);
        assert_eq!(plan.bytes_after, 200);
        assert_eq!(plan.temps.len(), 1, "a temporary file five days old goes");
        // The uses of what stays are kept; the log is folded.
        assert_eq!(plan.folds.len(), 1);
        assert_eq!(plan.folds[0].2, vec![("a".repeat(64), UNIX_EPOCH + Duration::from_secs(9 * DAY))]);
        // Unused for a year: nothing.
        assert!(plan_for(365).entries.is_empty());
    }

    fn plan_for(days: u64) -> Plan {
        plan(&store(), UNIX_EPOCH + Duration::from_secs(10 * DAY), Some(Duration::from_secs(days * DAY)), None)
    }

    #[test]
    fn gc_to_a_size_removes_the_least_recently_used_first() {
        let now = UNIX_EPOCH + Duration::from_secs(10 * DAY);
        let plan = plan(&store(), now, Some(Duration::from_secs(365 * DAY)), Some(150));
        // Last uses: a day 9, b day 2, c day 8: b first, then c.
        assert_eq!(keys(&plan), ['b', 'c']);
        assert_eq!(plan.bytes_after, 100);
        // The size alone, with no age, decides the same.
        assert_eq!(keys(&super::plan(&store(), now, None, Some(150))), ['b', 'c']);
        assert!(super::plan(&store(), now, None, None).entries.is_empty());
    }

    #[test]
    fn reads_sizes_and_ages() {
        assert_eq!(parse_size("200G").unwrap(), 200 << 30);
        assert_eq!(parse_size("1.5T").unwrap(), 3 << 39);
        assert_eq!(parse_size("512 MB").unwrap(), 512 << 20);
        assert_eq!(parse_size("1000").unwrap(), 1000);
        assert!(parse_size("lots").is_err() && parse_size("5X").is_err());
        assert_eq!(parse_age("30d").unwrap(), Duration::from_secs(30 * DAY));
        assert_eq!(parse_age("2w").unwrap(), Duration::from_secs(14 * DAY));
        assert_eq!(parse_age("6m").unwrap(), Duration::from_secs(180 * DAY));
        assert!(parse_age("30").is_err() && parse_age("d").is_err() && parse_age("3x").is_err());
        // Too large to count is an error, not an age that wrapped round.
        assert!(parse_age("99999999999999999y").is_err());
        assert_eq!(human(1536), "1.5 KB");
        assert_eq!(human(10), "10 bytes");
    }

    /// The walk, the log, and carrying out a plan, on a real store.
    #[test]
    fn walks_logs_and_collects_a_real_store() {
        use super::super::key::Parts;
        use super::super::store::{About, NewEntry};
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let store = Store::new(&root, "plato").unwrap();
        let object = tmp.path().join("a.o");
        fs::write(&object, b"object").unwrap();
        let mut published = Vec::new();
        for seed in ["one", "two"] {
            let parts = Parts {
                platform: seed.into(),
                compiler: "c".into(),
                arguments: "a".into(),
                environment: "e".into(),
                text: "t".into(),
                files: "f".into(),
            };
            let key = parts.key();
            let about = About { unit: None, compiler: "gcc".into(), relocatable: true, cactup: "t".into(), host: "h".into() };
            store.publish(&NewEntry { key: &key, parts: &parts, object: &object, stdout: b"", stderr: b"", modules: &[], about }).unwrap();
            published.push(key);
        }
        log_use(&store, &published[..1]).unwrap();
        fs::create_dir_all(root.join("v0/old")).unwrap();
        fs::write(root.join("v0/old/x"), b"12345").unwrap();

        let scan = scan(&root).unwrap();
        assert_eq!(scan.machines.len(), 1);
        assert_eq!(scan.machines[0].entries.len(), 2);
        assert_eq!(scan.machines[0].logs.len(), 1);
        assert_eq!(scan.other_formats, vec![root.join("v0")]);
        let now = crate::lock::fileserver_now(tmp.path()).unwrap();

        // Nothing is old enough: nothing goes, and the log is folded.
        let plan = plan(&scan, now, Some(Duration::from_secs(DAY)), None);
        assert!(plan.entries.is_empty());
        carry_out(&plan).unwrap();
        let used: Vec<String> = fs::read_dir(root.join(format!("v{FORMAT}/plato/used"))).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(used.len(), 1, "{used:?}");
        assert!(used[0].ends_with(".times"), "{used:?}");
        // The folded log still knows the use.
        assert_eq!(super::scan(&root).unwrap().machines[0].last_uses().len(), 1);

        // Everything is "old" a year from now: both go.
        let later = now + Duration::from_secs(365 * DAY);
        let plan = super::plan(&super::scan(&root).unwrap(), later, Some(Duration::from_secs(DAY)), None);
        let done = carry_out(&plan).unwrap();
        assert_eq!(done.entries, 2);
        assert!(super::scan(&root).unwrap().machines[0].entries.is_empty());
        // The other format is not touched.
        assert!(root.join("v0/old/x").exists());
    }

    /// A use log anyone who reads the store can read; and one that cannot be
    /// read stops the walk, rather than passing for a log of nothing.
    #[test]
    fn a_use_log_is_readable_and_an_unreadable_one_stops_the_walk() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let store = Store::new(&root, "plato").unwrap();
        log_use(&store, &["a".repeat(64)]).unwrap();
        let log = fs::read_dir(root.join(format!("v{FORMAT}/plato/used"))).unwrap().next().unwrap().unwrap().path();
        let status = fs::read_to_string("/proc/self/status").unwrap();
        let umask = u32::from_str_radix(status.lines().find_map(|l| l.strip_prefix("Umask:")).unwrap().trim(), 8).unwrap();
        assert_eq!(fs::metadata(&log).unwrap().permissions().mode() & 0o777, 0o666 & !umask);
        stamp_size(&root, 5).unwrap();
        assert_eq!(fs::metadata(root.join(format!("v{FORMAT}/size"))).unwrap().permissions().mode() & 0o777, 0o666 & !umask);

        fs::set_permissions(&log, fs::Permissions::from_mode(0)).unwrap();
        if fs::read(&log).is_ok() {
            return; // run as root: nothing is unreadable
        }
        let scan = scan(&root).unwrap();
        assert_eq!(scan.unreadable_logs.len(), 1, "{:?}", scan.unreadable_logs);
        assert_eq!(scan.unreadable_logs[0].0, log);
    }

    #[test]
    fn a_build_adds_to_the_size_and_never_walks() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        // Never measured: not known, and nothing written that would pass for
        // a measurement.
        assert_eq!(add_to_size(&root, 100).unwrap(), None);
        assert!(!size_stamp(&root).exists());
        stamp_size(&root, 1000).unwrap();
        assert_eq!(add_to_size(&root, 0).unwrap(), Some(1000));
        assert_eq!(add_to_size(&root, 100).unwrap(), Some(1100));
        assert_eq!(add_to_size(&root, 50).unwrap(), Some(1150));
    }
}
