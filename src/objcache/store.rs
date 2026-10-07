//! The store (§18.7): one file per key, shared by every build of the
//! instance, on whatever filesystem the instance lives on — NFS and Lustre
//! included, where `flock` cannot be trusted.
//!
//! So nothing here locks, and nothing keeps an index. Every write is a new
//! file under a name of its own, made whole and synced before the one
//! shared step, `link(2)`, gives it its entry's name. A name that exists
//! therefore names a whole entry, and an entry is never changed again.
//! Whoever links first has published the object of that key; whoever comes
//! second has the same object and loses nothing.
//!
//! Reading trusts nothing: every restore checks the whole entry against its
//! size and checksum while copying it, and an entry that does not hold up
//! is a miss (and removed). The object is copied, never linked: a later
//! compile writes into the build's object in place, and must not write into
//! the store.
//!
//! Several cactup builds share one store at once (a queued job runs the
//! build it was submitted with, for months). So the lengths an entry is
//! cut by are outside its header, and the header is read only once size
//! and checksum have shown the entry whole: an entry this cactup cannot
//! read the header of was written by another, and is left alone. Any
//! change to what an entry holds or how it is read bumps [`FORMAT`], which
//! puts the new entries in a directory of their own.

use super::hash::Checksum;
use super::key::{Parts, KEY_LABEL};
use crate::Res;
use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, Metadata};
use std::io::{self, BufRead, BufReader, BufWriter, ErrorKind, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The entry format, and the name of the directory its entries live in. A
/// cactup with another format writes beside this one and never reads it
/// (no backward compatibility, §2.4).
pub const FORMAT: u32 = 1;

/// An entry's first line.
const MAGIC: &[u8] = b"cactup build cache entry\n";

/// The largest header a reader accepts: a few hundred bytes in practice, so
/// a length beyond this is a damaged entry, not a reason to allocate.
const MAX_HEADER: u64 = 64 * 1024;

/// The longest lengths line: four 20-digit numbers, three spaces, a newline.
const MAX_LENGTHS_LINE: u64 = 4 * 20 + 3 + 1;

/// Reads and writes of entries go in large pieces: on NFS every small one
/// is a round trip.
const BUFFER: usize = 1 << 20;

/// What an entry says about itself, between the lengths line and the blobs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct Header {
    format: u32,
    /// The label the key was made under (`key::KEY_LABEL`): a whole entry
    /// whose parts digest to another key under this cactup's label may
    /// simply be another cactup's.
    label: String,
    key: String,
    parts: Parts,
    about: About,
}

/// What an entry records for people (`cache stats`, someone reading one by
/// hand). Nothing a restore checks or uses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct About {
    /// The object's name below the configuration's `build` directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// The compiler as the recipe named it.
    pub compiler: String,
    /// Is the key free of where the installation is?
    pub relocatable: bool,
    /// The cactup that published it, and the host it compiled on.
    pub cactup: String,
    pub host: String,
}

/// What a publisher hands the store: the compile that succeeded, under the
/// key that still held after it.
pub struct NewEntry<'a> {
    pub key: &'a str,
    pub parts: &'a Parts,
    /// The object the compile wrote.
    pub object: &'a Path,
    pub stdout: &'a [u8],
    pub stderr: &'a [u8],
    pub about: About,
}

/// How a publish ended, when it did not fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Published {
    /// This call linked the entry.
    Stored,
    /// The key had an entry already (perhaps linked a moment ago by another
    /// build): the same object, so there was nothing to add.
    AlreadyThere,
}

/// What the compiler wrote besides the object, as a hit replays it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Messages {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Why a restore found no object to give.
#[derive(Debug)]
pub enum Miss {
    /// No entry under the key.
    Absent,
    /// An entry that does not hold up, and whether it was removed (it is
    /// not when its name leads elsewhere by now, or cannot be removed).
    Invalid { why: String, removed: bool },
    /// A whole entry whose header this cactup cannot read: another
    /// cactup's, written in the same format by a different build. Left
    /// alone.
    Foreign(String),
    /// An entry that could not be read, for a reason that says nothing
    /// about it (an I/O error, a stale NFS handle): left alone.
    Unreadable(String),
    /// The object could not be written where the compile would write it.
    CannotWrite(String),
}

impl std::fmt::Display for Miss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Absent => write!(f, "no entry"),
            Self::Invalid { why, removed: true } => write!(f, "an invalid entry, removed: {why}"),
            Self::Invalid { why, removed: false } => write!(f, "an invalid entry: {why}"),
            Self::Foreign(why) => write!(f, "an entry this cactup cannot read: {why}"),
            Self::Unreadable(why) => write!(f, "the entry could not be read: {why}"),
            Self::CannotWrite(why) => write!(f, "the object could not be written: {why}"),
        }
    }
}

/// One machine's part of the store: `<root>/v<FORMAT>/<machine>`.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// The part of the store under `root` for `machine`. Nothing is created
    /// until something is published. A machine name that is not a plain
    /// file name would put entries somewhere else, so it keeps the build
    /// out of the store.
    pub fn new(root: &Path, machine: &str) -> Res<Self> {
        let plain = !machine.is_empty()
            && !machine.starts_with('.')
            && !machine.contains(['/', '\0', '\n']);
        if !plain {
            bail!("the machine name \"{machine}\" cannot name a directory of the build cache");
        }
        Ok(Self { dir: root.join(format!("v{FORMAT}")).join(machine) })
    }

    /// Where the entry for `key` is. `None` for anything but a key (64
    /// lowercase hex digits), which could name a path anywhere.
    pub fn entry_path(&self, key: &str) -> Option<PathBuf> {
        let is_key = key.len() == 64 && key.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        is_key.then(|| self.dir.join(&key[..2]).join(key))
    }

    /// Publish the object of a compile under its key (§18.7, "Publishing").
    /// An error leaves the store as it was, but for a `.tmp-` file if this
    /// process is stopped half way; the build goes on either way.
    pub fn publish(&self, entry: &NewEntry) -> Res<Published> {
        let path = self.entry_path(entry.key).with_context(|| format!("\"{}\" is not a key", entry.key))?;
        match path.symlink_metadata() {
            Ok(meta) if meta.is_file() => return Ok(Published::AlreadyThere),
            Ok(_) => bail!("{} is there and is not an entry", path.display()),
            Err(_) => {}
        }
        let dir = path.parent().expect("an entry is inside its directory");
        fs::create_dir_all(dir).with_context(|| format!("Failed to create {}", dir.display()))?;

        let mut object = File::open(entry.object).with_context(|| format!("Failed to open {}", entry.object.display()))?;
        let before = object.metadata().with_context(|| format!("Failed to look at {}", entry.object.display()))?;
        if !before.is_file() {
            bail!("{} is not a regular file", entry.object.display());
        }
        let header = Header {
            format: FORMAT,
            label: KEY_LABEL.to_owned(),
            key: entry.key.to_owned(),
            parts: entry.parts.clone(),
            about: entry.about.clone(),
        };
        let header = toml::to_string(&header).context("Failed to write an entry's header")?;
        let lengths = format!("{} {} {} {}\n", header.len(), before.len(), entry.stdout.len(), entry.stderr.len());

        let temp = tempfile::Builder::new()
            .prefix(&format!(".tmp-{}-", entry.key))
            .tempfile_in(dir)
            .with_context(|| format!("Failed to create a temporary file in {}", dir.display()))?;
        let mut out = Summed { inner: BufWriter::with_capacity(BUFFER, temp.as_file()), sum: Checksum::new() };
        let mut write = |out: &mut Summed<BufWriter<&File>>| -> io::Result<()> {
            out.write_all(MAGIC)?;
            out.write_all(lengths.as_bytes())?;
            out.write_all(header.as_bytes())?;
            // Exactly as many bytes as the lengths say.
            let copied = io::copy(&mut (&mut object).take(before.len()), out)?;
            if copied != before.len() {
                return Err(io::Error::new(ErrorKind::UnexpectedEof, "the object got shorter while it was copied"));
            }
            out.write_all(entry.stdout)?;
            out.write_all(entry.stderr)
        };
        write(&mut out).with_context(|| format!("Failed to write {}", temp.path().display()))?;
        // An object that changed while it was copied (grown, rewritten) is
        // not the one the compile wrote.
        let after = object.metadata().with_context(|| format!("Failed to look at {}", entry.object.display()))?;
        if (after.len(), after.mtime(), after.mtime_nsec()) != (before.len(), before.mtime(), before.mtime_nsec()) {
            bail!("{} changed while it was copied", entry.object.display());
        }
        let Summed { inner, sum } = out;
        let mut file = inner.into_inner().map_err(|e| e.into_error()).context("Failed to write an entry")?;
        file.write_all(format!("{}\n", sum.hex()).as_bytes()).context("Failed to write an entry")?;
        // On disk before it has a name: a name that exists names a whole
        // entry, also after a crash. Synced before it is made read-only, so
        // that a server checking permissions when the data reaches it has
        // nothing left to refuse.
        file.sync_all().context("Failed to sync an entry to disk")?;
        file.set_permissions(fs::Permissions::from_mode(0o444)).context("Failed to make an entry read-only")?;

        let linked = fs::hard_link(temp.path(), &path);
        // NFS can lose the reply to a link that happened; the temporary
        // file's link count says whether it did (as in `lock::LinkLock`).
        let nlink = temp.as_file().metadata().map(|meta| meta.nlink()).unwrap_or(1);
        match linked {
            Ok(()) => Ok(Published::Stored),
            Err(_) if nlink == 2 => Ok(Published::Stored),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(Published::AlreadyThere),
            Err(e) => Err(e).with_context(|| format!("Failed to link {}", path.display())),
        }
        // The temporary file's own name goes when `temp` is dropped.
    }

    /// Remove the entry of `key`, which audit mode has shown to be wrong
    /// (§18.8): the next publish of the key can put the right one there.
    pub fn remove(&self, key: &str) -> Res<()> {
        let path = self.entry_path(key).with_context(|| format!("\"{key}\" is not a key"))?;
        match fs::remove_file(&path) {
            Err(e) if e.kind() != ErrorKind::NotFound => Err(e).with_context(|| format!("Failed to remove {}", path.display())),
            _ => Ok(()),
        }
    }

    /// Restore the object of `key` to `object`, where the compile would
    /// have written it, if the store has a valid entry for it (§18.7,
    /// "Restoring"). On a miss nothing is left at `object` that was not
    /// there before.
    pub fn restore(&self, key: &str, object: &Path) -> Result<Messages, Miss> {
        let path = self.entry_path(key).ok_or_else(|| Miss::Invalid { why: format!("\"{key}\" is not a key"), removed: false })?;
        let unreadable = |e: io::Error| Miss::Unreadable(format!("{}: {e}", path.display()));
        // Looked at before it is opened: a FIFO there would block the open.
        let meta = match path.symlink_metadata() {
            Ok(meta) => meta,
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => return Err(Miss::Absent),
            Err(e) => return Err(unreadable(e)),
        };
        if !meta.is_file() {
            let removed = invalidate(&path, &meta);
            return Err(Miss::Invalid { why: "it is not a regular file".to_owned(), removed });
        }
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => return Err(Miss::Absent),
            Err(e) => return Err(unreadable(e)),
        };
        let meta = file.metadata().map_err(unreadable)?;
        match read_into(file, &meta, key, object) {
            Ok(messages) => Ok(messages),
            Err(Fault::Invalid(why)) => {
                let removed = invalidate(&path, &meta);
                Err(Miss::Invalid { why, removed })
            }
            Err(Fault::Foreign(why)) => Err(Miss::Foreign(why)),
            Err(Fault::Io(e)) => Err(unreadable(e)),
            Err(Fault::Output(why)) => Err(Miss::CannotWrite(why)),
        }
    }
}

/// A writer that digests what goes through it.
struct Summed<W> {
    inner: W,
    sum: Checksum,
}

impl<W: Write> Write for Summed<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.sum.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// What can go wrong reading an entry, by what it says about the entry.
enum Fault {
    /// The entry's bytes are not a whole entry, or not one for its key.
    Invalid(String),
    /// A whole entry, with a header this cactup does not read.
    Foreign(String),
    /// Reading failed for a reason of its own; the entry may be fine.
    Io(io::Error),
    /// The entry may be fine; the object could not be put in place.
    Output(String),
}

impl From<io::Error> for Fault {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Read the entry `file` (whose metadata is `meta`) for `key`, copying its
/// object to a temporary file beside `object`, and move that onto `object`
/// once the whole entry has been checked.
///
/// The order is the point: the size against the lengths line, then the
/// checksum over every byte, and only then the header. So an entry whose
/// header this cactup cannot read, but which is whole, is another cactup's
/// and not damage.
fn read_into(file: File, meta: &Metadata, key: &str, object: &Path) -> Result<Messages, Fault> {
    let invalid = |why: &str| Fault::Invalid(why.to_owned());
    let mut reader = Digesting { inner: BufReader::with_capacity(BUFFER, file), sum: Checksum::new() };
    // Until the size has been checked, an entry that ends early is short.
    let early = |e: io::Error| match e.kind() {
        ErrorKind::UnexpectedEof => Fault::Invalid("it ends early".to_owned()),
        _ => Fault::Io(e),
    };

    let mut magic = vec![0; MAGIC.len()];
    reader.read_exact(&mut magic).map_err(early)?;
    if magic != MAGIC {
        return Err(invalid("it does not begin as an entry does"));
    }
    let mut line = Vec::new();
    (&mut reader).take(MAX_LENGTHS_LINE).read_until(b'\n', &mut line)?;
    let lengths: Vec<u64> = std::str::from_utf8(&line)
        .ok()
        .and_then(|line| line.strip_suffix('\n'))
        .map(|line| line.split(' ').map(|n| n.bytes().all(|b| b.is_ascii_digit()).then(|| n.parse().ok()).flatten()).collect())
        .and_then(|lengths: Vec<Option<u64>>| lengths.into_iter().collect::<Option<Vec<u64>>>())
        .filter(|lengths| lengths.len() == 4 && lengths[0] <= MAX_HEADER)
        .ok_or_else(|| invalid("its lengths cannot be read"))?;
    let [header_len, object_len, stdout_len, stderr_len] = lengths[..] else { unreachable!() };
    let expected = [MAGIC.len() as u64, line.len() as u64, header_len, object_len, stdout_len, stderr_len, 65]
        .iter()
        .try_fold(0u64, |sum, part| sum.checked_add(*part));
    if expected != Some(meta.len()) {
        return Err(invalid("its size is not what its lengths say"));
    }
    let mut header = vec![0; header_len as usize];
    read_exact(&mut reader, &mut header)?;

    // The object, into a temporary file beside where it goes, created as a
    // compiler creates its output (`0666` less the umask, and whatever a
    // default ACL of the directory adds).
    let dir = object.parent().filter(|dir| !dir.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = object.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let output = |e: io::Error| Fault::Output(format!("{}: {e}", object.display()));
    let temp = tempfile::Builder::new()
        .prefix(&format!(".{name}.cactup-"))
        .permissions(fs::Permissions::from_mode(0o666))
        .tempfile_in(dir)
        .map_err(output)?;
    {
        let mut writer = BufWriter::with_capacity(BUFFER, temp.as_file());
        let copied = io::copy(&mut (&mut reader).take(object_len), &mut writer);
        match copied {
            Ok(n) if n == object_len => {}
            Ok(_) => return Err(Fault::Io(ErrorKind::UnexpectedEof.into())),
            // Reading the entry and writing the object fail alike here;
            // which it was, the entry's own read below would not tell.
            Err(e) => return Err(Fault::Output(format!("{}: {e}", object.display()))),
        }
        writer.flush().map_err(output)?;
    }
    let mut stdout = vec![0; stdout_len as usize];
    read_exact(&mut reader, &mut stdout)?;
    let mut stderr = vec![0; stderr_len as usize];
    read_exact(&mut reader, &mut stderr)?;
    let Digesting { inner: mut rest, sum } = reader;
    let mut written = [0; 65];
    read_exact(&mut rest, &mut written)?;
    if written[..64] != *sum.hex().as_bytes() || written[64] != b'\n' {
        return Err(invalid("its checksum does not match its content"));
    }

    // Whole. Now whose, and for which key.
    let header: Header = std::str::from_utf8(&header)
        .ok()
        .and_then(|text| toml::from_str(text).ok())
        .ok_or_else(|| Fault::Foreign("its header is not one this cactup writes".to_owned()))?;
    if header.format != FORMAT {
        return Err(Fault::Foreign(format!("it says it is of format {}", header.format)));
    }
    if header.label != KEY_LABEL {
        return Err(Fault::Foreign(format!("its key was made under the label {}", header.label)));
    }
    if header.key != key || header.parts.key() != key {
        return Err(invalid("it is the entry of another key"));
    }
    temp.persist(object).map_err(|e| output(e.error))?;
    Ok(Messages { stdout, stderr })
}

/// `read_exact`, where running out of bytes early is an I/O fault, not a
/// damaged entry: the size was checked against the lengths before, so an
/// entry that ends early is one that changed while it was read, which an
/// entry never does.
fn read_exact(reader: &mut impl Read, buf: &mut [u8]) -> Result<(), Fault> {
    reader.read_exact(buf).map_err(Fault::Io)
}

/// A reader that digests what comes through it.
struct Digesting<R> {
    inner: R,
    sum: Checksum,
}

impl<R: Read> Read for Digesting<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.sum.update(&buf[..n]);
        Ok(n)
    }
}

impl<R: BufRead> BufRead for Digesting<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.inner.fill_buf()
    }

    fn consume(&mut self, amount: usize) {
        if let Ok(buf) = self.inner.fill_buf() {
            self.sum.update(&buf[..amount.min(buf.len())]);
        }
        self.inner.consume(amount);
    }
}

/// Remove the invalid entry at `path`, which was read as the file `read`,
/// so that the next compile of its key can publish a good one — but only if
/// the name still leads to that file: another build may have removed it and
/// published anew. A good entry that slips in between the look and the
/// removal is removed too, which costs one miss and nothing else. Whether
/// it was removed.
fn invalidate(path: &Path, read: &Metadata) -> bool {
    let same = path.symlink_metadata().is_ok_and(|now| now.dev() == read.dev() && now.ino() == read.ino());
    same && fs::remove_file(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This process's file-creation mask (`umask(2)` can only read it by
    /// changing it).
    fn umask() -> u32 {
        let status = fs::read_to_string("/proc/self/status").unwrap();
        let mask = status.lines().find_map(|line| line.strip_prefix("Umask:")).unwrap();
        u32::from_str_radix(mask.trim(), 8).unwrap()
    }

    fn parts(seed: &str) -> Parts {
        Parts {
            platform: format!("{seed}-platform"),
            compiler: "compiler".into(),
            arguments: "arguments".into(),
            environment: "environment".into(),
            text: "text".into(),
            files: "files".into(),
        }
    }

    fn about() -> About {
        About {
            unit: Some("Thorn/a.c.o".into()),
            compiler: "gcc".into(),
            relocatable: true,
            cactup: "cactup test".into(),
            host: "testhost".into(),
        }
    }

    /// A store in a temporary directory, an object, and the entry for it.
    struct Fixture {
        tmp: tempfile::TempDir,
        store: Store,
        parts: Parts,
        key: String,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let store = Store::new(&tmp.path().join("cache"), "plato").unwrap();
            let parts = parts("one");
            let key = parts.key();
            fs::create_dir_all(tmp.path().join("build")).unwrap();
            Self { tmp, store, parts, key }
        }

        fn object(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.tmp.path().join("build").join(name);
            fs::write(&path, bytes).unwrap();
            path
        }

        fn publish(&self, object: &Path, stderr: &[u8]) -> Res<Published> {
            self.store.publish(&NewEntry {
                key: &self.key,
                parts: &self.parts,
                object,
                stdout: b"",
                stderr,
                about: about(),
            })
        }

        fn entry(&self) -> PathBuf {
            self.store.entry_path(&self.key).unwrap()
        }

        /// Rewrite the entry's bytes (it is read-only, as published).
        fn damage(&self, change: impl FnOnce(&mut Vec<u8>)) {
            let mut bytes = fs::read(self.entry()).unwrap();
            change(&mut bytes);
            fs::remove_file(self.entry()).unwrap();
            fs::write(self.entry(), bytes).unwrap();
        }

        /// What a directory holds, besides `keep`.
        fn leftovers(&self, dir: &Path, keep: &[&Path]) -> Vec<PathBuf> {
            let mut left: Vec<PathBuf> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
            left.retain(|path| !keep.contains(&path.as_path()));
            left
        }
    }

    #[test]
    fn an_entry_goes_in_once_and_comes_out_whole() {
        let fx = Fixture::new();
        let object = fx.object("a.c.o", b"\x7fELF object bytes");
        assert_eq!(fx.publish(&object, b"a.c:1: warning: something\n").unwrap(), Published::Stored);
        // Under its key, read-only, with nothing else left beside it.
        let entry = fx.entry();
        assert_eq!(entry, fx.tmp.path().join(format!("cache/v1/plato/{}/{}", &fx.key[..2], fx.key)));
        assert_eq!(fs::metadata(&entry).unwrap().permissions().mode() & 0o777, 0o444);
        assert_eq!(fx.leftovers(entry.parent().unwrap(), &[&entry]), Vec::<PathBuf>::new());
        // Never again: the first publisher's entry stands.
        let other = fx.object("b.c.o", b"other bytes");
        assert_eq!(fx.publish(&other, b"").unwrap(), Published::AlreadyThere);

        let restored = fx.tmp.path().join("build/restored.c.o");
        let messages = fx.store.restore(&fx.key, &restored).unwrap();
        assert_eq!(fs::read(&restored).unwrap(), b"\x7fELF object bytes");
        assert_eq!(messages, Messages { stdout: vec![], stderr: b"a.c:1: warning: something\n".to_vec() });
        // Writable as a compiler's output is, and nothing else in the
        // build directory but the three objects.
        assert_eq!(fs::metadata(&restored).unwrap().permissions().mode() & 0o777, 0o666 & !umask());
        assert_eq!(fx.leftovers(&fx.tmp.path().join("build"), &[&object, &other, &restored]), Vec::<PathBuf>::new());
        // Over an object that is there already, too.
        fs::write(&restored, b"stale").unwrap();
        fx.store.restore(&fx.key, &restored).unwrap();
        assert_eq!(fs::read(&restored).unwrap(), b"\x7fELF object bytes");
    }

    #[test]
    fn an_empty_object_and_large_messages_survive() {
        let fx = Fixture::new();
        let object = fx.object("empty.o", b"");
        let stderr: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        fx.publish(&object, &stderr).unwrap();
        let restored = fx.tmp.path().join("build/r.o");
        let messages = fx.store.restore(&fx.key, &restored).unwrap();
        assert_eq!(fs::read(&restored).unwrap(), b"");
        assert_eq!(messages.stderr, stderr);
    }

    #[test]
    fn no_entry_is_a_miss_that_writes_nothing() {
        let fx = Fixture::new();
        let restored = fx.tmp.path().join("build/r.o");
        assert!(matches!(fx.store.restore(&fx.key, &restored), Err(Miss::Absent)));
        assert!(!restored.exists());
        assert!(!fx.tmp.path().join("cache").exists(), "a miss creates nothing");
    }

    #[test]
    fn only_keys_name_entries_and_only_plain_names_name_machines() {
        let fx = Fixture::new();
        for not_a_key in ["", "../../etc/passwd", &"A".repeat(64), &"0".repeat(63), &format!("{}/", "0".repeat(63))] {
            assert_eq!(fx.store.entry_path(not_a_key), None, "{not_a_key}");
        }
        for machine in ["", ".", "..", ".hidden", "a/b", "a\nb"] {
            assert!(Store::new(Path::new("/c"), machine).is_err(), "{machine:?}");
        }
        assert!(Store::new(Path::new("/c"), "db1.hpc.lsu.edu").is_ok());
    }

    /// Every way an entry can be damaged is a miss, removes the entry, and
    /// leaves the object where it would be untouched.
    #[test]
    fn a_damaged_entry_is_a_miss_and_is_removed() {
        let fx = Fixture::new();
        let object = fx.object("a.c.o", b"the object's bytes, long enough to flip one of them");
        let damages: Vec<(&str, Box<dyn Fn(&mut Vec<u8>)>)> = vec![
            ("one byte of the object", Box::new(|b: &mut Vec<u8>| {
                let at = b.windows(6).position(|w| w == b"object").unwrap();
                b[at] ^= 1;
            })),
            ("cut short", Box::new(|b: &mut Vec<u8>| b.truncate(b.len() - 10))),
            ("one byte more", Box::new(|b: &mut Vec<u8>| b.push(b'x'))),
            ("the checksum", Box::new(|b: &mut Vec<u8>| {
                let at = b.len() - 2;
                b[at] = if b[at] == b'0' { b'1' } else { b'0' };
            })),
            ("the magic line", Box::new(|b: &mut Vec<u8>| b[0] = b'C')),
            ("the lengths", Box::new(|b: &mut Vec<u8>| b[MAGIC.len()] = b'x')),
            ("lengths beyond reason", Box::new(|b: &mut Vec<u8>| {
                let line_end = MAGIC.len() + b[MAGIC.len()..].iter().position(|c| *c == b'\n').unwrap();
                b.splice(MAGIC.len()..line_end, b"99999999999999999999".iter().copied());
            })),
            ("empty", Box::new(|b: &mut Vec<u8>| b.clear())),
        ];
        for (what, damage) in damages {
            let _ = fs::remove_file(fx.entry());
            fx.publish(&object, b"").unwrap();
            fx.damage(damage);
            let restored = fx.tmp.path().join("build/r.o");
            fs::write(&restored, b"what was there").unwrap();
            let miss = fx.store.restore(&fx.key, &restored).unwrap_err();
            assert!(matches!(miss, Miss::Invalid { removed: true, .. }), "{what}: {miss}");
            assert!(!fx.entry().exists(), "{what}: the entry was left in place");
            assert_eq!(fs::read(&restored).unwrap(), b"what was there", "{what}");
            assert_eq!(fx.leftovers(&fx.tmp.path().join("build"), &[&object, &restored]), Vec::<PathBuf>::new(), "{what}");
        }
    }

    /// An entry under the wrong name, or one whose header was rewritten to
    /// match a name, is the entry of another key.
    #[test]
    fn an_entry_answers_for_its_own_key_only() {
        let fx = Fixture::new();
        let object = fx.object("a.c.o", b"bytes");
        fx.publish(&object, b"").unwrap();
        let other = parts("two").key();
        let misplaced = fx.store.entry_path(&other).unwrap();
        fs::create_dir_all(misplaced.parent().unwrap()).unwrap();
        fs::copy(fx.entry(), &misplaced).unwrap();
        let miss = fx.store.restore(&other, &fx.tmp.path().join("build/r.o")).unwrap_err();
        assert!(matches!(&miss, Miss::Invalid { why, .. } if why.contains("another key")), "{miss}");
        assert!(!misplaced.exists());
        assert!(fx.entry().exists(), "the entry under its own name stays");

        // A header claiming the other key, with a checksum to match, still
        // has the parts of the first: the key is the digest of its parts.
        let text = fs::read(fx.entry()).unwrap();
        let body = &text[..text.len() - 65];
        let forged: Vec<u8> = String::from_utf8_lossy(body).replace(&fx.key, &other).into_bytes();
        let mut sum = Checksum::new();
        sum.update(&forged);
        let mut forged = forged;
        forged.extend(format!("{}\n", sum.hex()).bytes());
        fs::write(&misplaced, forged).unwrap();
        let miss = fx.store.restore(&other, &fx.tmp.path().join("build/r.o")).unwrap_err();
        assert!(matches!(&miss, Miss::Invalid { why, .. } if why.contains("another key")), "{miss}");
    }

    /// A whole entry (size and checksum right) whose header this cactup
    /// cannot read was written by another cactup in the same format: a
    /// miss, and left alone. One with anything less is damage.
    #[test]
    fn a_whole_entry_with_a_header_of_another_cactup_is_left_alone() {
        let fx = Fixture::new();
        let object = fx.object("a.c.o", b"bytes");
        // Rebuild the entry with the header edited, its length and checksum
        // made to match.
        let rewrite = |edit: &dyn Fn(String) -> String| {
            let text = fs::read(fx.entry()).unwrap();
            let line_end = MAGIC.len() + text[MAGIC.len()..].iter().position(|b| *b == b'\n').unwrap() + 1;
            let lengths: Vec<usize> = std::str::from_utf8(&text[MAGIC.len()..line_end - 1]).unwrap().split(' ').map(|n| n.parse().unwrap()).collect();
            let header = String::from_utf8(text[line_end..line_end + lengths[0]].to_vec()).unwrap();
            let edited = edit(header.clone());
            assert_ne!(edited, header, "the edit did nothing");
            let line = format!("{} {} {} {}\n", edited.len(), lengths[1], lengths[2], lengths[3]);
            let mut body = [MAGIC, line.as_bytes(), edited.as_bytes()].concat();
            body.extend_from_slice(&text[line_end + lengths[0]..text.len() - 65]);
            let mut sum = Checksum::new();
            sum.update(&body);
            body.extend(format!("{}\n", sum.hex()).bytes());
            fs::remove_file(fx.entry()).unwrap();
            fs::write(fx.entry(), body).unwrap();
        };
        let edits: [(&str, &dyn Fn(String) -> String); 5] = [
            // Another cactup's key label: its parts digest to another key
            // under this one's, which is not damage.
            ("another label", &|h| h.replacen("label = \"key-5\"\n", "label = \"key-2\"\n", 1)),
            ("a field more", &|h| h.replacen("format = 1\n", "format = 1\nextra = 2\n", 1)),
            ("a field more in the parts", &|h| h.replacen("[parts]\n", "[parts]\nextra = \"x\"\n", 1)),
            ("a field more about it", &|h| h.replacen("[about]\n", "[about]\nextra = \"x\"\n", 1)),
            ("a field less", &|h| h.replacen("relocatable = true\n", "", 1)),
        ];
        for (what, edit) in edits {
            let _ = fs::remove_file(fx.entry());
            fx.publish(&object, b"").unwrap();
            rewrite(edit);
            let restored = fx.tmp.path().join("build/r.o");
            let miss = fx.store.restore(&fx.key, &restored).unwrap_err();
            assert!(matches!(&miss, Miss::Foreign(_)), "{what}: {miss}");
            assert!(fx.entry().exists(), "{what}: another cactup's entry was removed");
            assert!(!restored.exists(), "{what}");
        }
        // Another format, likewise.
        let _ = fs::remove_file(fx.entry());
        fx.publish(&object, b"").unwrap();
        rewrite(&|h| h.replacen("format = 1\n", "format = 7\n", 1));
        assert!(matches!(fx.store.restore(&fx.key, &fx.tmp.path().join("build/r.o")), Err(Miss::Foreign(_))));
    }

    /// Something other than a file where an entry should be: no restore
    /// blocks on it, and no publish takes it for an entry.
    #[test]
    fn what_is_not_a_file_is_not_an_entry() {
        let fx = Fixture::new();
        let object = fx.object("a.c.o", b"bytes");
        fs::create_dir_all(fx.entry().parent().unwrap()).unwrap();
        let fifo = std::process::Command::new("mkfifo").arg(fx.entry()).status().unwrap();
        assert!(fifo.success());
        let miss = fx.store.restore(&fx.key, &fx.tmp.path().join("build/r.o")).unwrap_err();
        assert!(matches!(&miss, Miss::Invalid { removed: true, .. }), "{miss}");
        fs::create_dir(fx.entry()).unwrap();
        let err = fx.publish(&object, b"").unwrap_err().to_string();
        assert!(err.contains("is not an entry"), "{err}");
        let miss = fx.store.restore(&fx.key, &fx.tmp.path().join("build/r.o")).unwrap_err();
        assert!(matches!(&miss, Miss::Invalid { removed: false, .. }), "{miss}");
    }

    #[test]
    fn an_invalid_entry_replaced_meanwhile_is_not_removed() {
        let fx = Fixture::new();
        let object = fx.object("a.c.o", b"bytes");
        fx.publish(&object, b"").unwrap();
        let read = fs::metadata(fx.entry()).unwrap();
        // Another build removed it and published anew.
        fs::remove_file(fx.entry()).unwrap();
        fx.publish(&object, b"").unwrap();
        invalidate(&fx.entry(), &read);
        assert!(fx.entry().exists());
        // The one that was read goes.
        invalidate(&fx.entry(), &fs::metadata(fx.entry()).unwrap());
        assert!(!fx.entry().exists());
    }

    #[test]
    fn a_restore_that_cannot_write_leaves_the_entry() {
        let fx = Fixture::new();
        let object = fx.object("a.c.o", b"bytes");
        fx.publish(&object, b"").unwrap();
        let miss = fx.store.restore(&fx.key, &fx.tmp.path().join("no such dir/r.o")).unwrap_err();
        assert!(matches!(miss, Miss::CannotWrite(_)), "{miss}");
        assert!(fx.entry().exists());
    }

    #[test]
    fn a_publish_that_cannot_happen_leaves_nothing() {
        let fx = Fixture::new();
        // No object.
        assert!(fx.publish(&fx.tmp.path().join("build/missing.o"), b"").is_err());
        // A directory where the object should be.
        assert!(fx.publish(&fx.tmp.path().join("build"), b"").is_err());
        let dir = fx.entry().parent().unwrap().to_owned();
        assert!(!fx.entry().exists());
        assert_eq!(fx.leftovers(&dir, &[]), Vec::<PathBuf>::new());
    }

    /// What a publisher stopped half way leaves behind: a `.tmp-` file
    /// holding part of an entry. No restore opens it, and the next publish
    /// goes through beside it.
    #[test]
    fn a_publish_cut_short_leaves_only_a_temporary_file() {
        let fx = Fixture::new();
        let dir = fx.entry().parent().unwrap().to_owned();
        fs::create_dir_all(&dir).unwrap();
        let partial = dir.join(format!(".tmp-{}-abc123", fx.key));
        fs::write(&partial, &MAGIC[..10]).unwrap();
        let restored = fx.tmp.path().join("build/r.o");
        assert!(matches!(fx.store.restore(&fx.key, &restored), Err(Miss::Absent)));
        let object = fx.object("a.c.o", b"bytes");
        assert_eq!(fx.publish(&object, b"").unwrap(), Published::Stored);
        fx.store.restore(&fx.key, &restored).unwrap();
        assert_eq!(fs::read(&restored).unwrap(), b"bytes");
        assert!(partial.exists(), "left for cache gc");
    }

    /// The store directory and object a [`child_publishes_and_restores`]
    /// works with, from its parent.
    const CHILD_DIR: &str = "CACTUP_STORE_TEST_DIR";

    /// Separate processes publishing and restoring one key at once, the way
    /// builds on several hosts do (but on one filesystem): every one ends
    /// well, and the store holds one entry and nothing else.
    #[test]
    fn processes_publishing_and_restoring_at_once_agree() {
        let tmp = tempfile::tempdir().unwrap();
        let bytes: Vec<u8> = (0..500_000u32).map(|i| (i * 13 % 256) as u8).collect();
        fs::write(tmp.path().join("object.o"), &bytes).unwrap();
        let children: Vec<_> = (0..8)
            .map(|_| {
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "objcache::store::tests::child_publishes_and_restores", "--ignored", "--nocapture"])
                    .env(CHILD_DIR, tmp.path())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect();
        for child in children {
            let out = child.wait_with_output().unwrap();
            let said = String::from_utf8_lossy(&out.stdout);
            assert!(out.status.success() && said.contains("child done"), "{said}{}", String::from_utf8_lossy(&out.stderr));
        }
        let store = Store::new(&tmp.path().join("cache"), "plato").unwrap();
        let entry = store.entry_path(&parts("one").key()).unwrap();
        let left: Vec<_> = fs::read_dir(entry.parent().unwrap()).unwrap().map(|e| e.unwrap().path()).collect();
        assert_eq!(left, vec![entry]);
    }

    /// Run only by [`processes_publishing_and_restoring_at_once_agree`].
    #[test]
    #[ignore]
    fn child_publishes_and_restores() {
        let Some(dir) = std::env::var_os(CHILD_DIR).map(PathBuf::from) else { return };
        let store = Store::new(&dir.join("cache"), "plato").unwrap();
        let parts = parts("one");
        let key = parts.key();
        let object = dir.join("object.o");
        let bytes = fs::read(&object).unwrap();
        let restored = dir.join(format!("r{}.o", std::process::id()));
        for _ in 0..40 {
            let entry = NewEntry { key: &key, parts: &parts, object: &object, stdout: b"out", stderr: b"err", about: about() };
            store.publish(&entry).unwrap();
            let messages = store.restore(&key, &restored).unwrap();
            assert_eq!(fs::read(&restored).unwrap(), bytes);
            assert_eq!(messages, Messages { stdout: b"out".to_vec(), stderr: b"err".to_vec() });
        }
        println!("child done");
    }

    /// Publishers, readers and invalidators of one key at once: entries come
    /// and go, and every restore is still a miss or the whole object.
    #[test]
    fn restores_hold_while_entries_are_removed_and_republished() {
        let fx = Fixture::new();
        let bytes: Vec<u8> = (0..400_000u32).map(|i| (i * 11 % 256) as u8).collect();
        let object = fx.object("big.o", &bytes);
        std::thread::scope(|scope| {
            for i in 0..12 {
                let (fx, object, bytes) = (&fx, &object, &bytes);
                scope.spawn(move || {
                    let restored = fx.tmp.path().join(format!("build/r{i}.o"));
                    for round in 0..30 {
                        match (i + round) % 3 {
                            0 => {
                                fx.publish(object, b"w").unwrap();
                            }
                            1 => {
                                if let Ok(meta) = fs::symlink_metadata(fx.entry()) {
                                    invalidate(&fx.entry(), &meta);
                                }
                            }
                            _ => match fx.store.restore(&fx.key, &restored) {
                                Ok(messages) => {
                                    assert_eq!(fs::read(&restored).unwrap(), *bytes);
                                    assert_eq!(messages.stderr, b"w");
                                }
                                Err(Miss::Absent) => {}
                                Err(miss) => panic!("{miss}"),
                            },
                        }
                    }
                });
            }
        });
        let build = fx.tmp.path().join("build");
        let temps: Vec<PathBuf> = fx.leftovers(&build, &[]).into_iter().filter(|p| p.to_string_lossy().contains(".cactup-")).collect();
        assert_eq!(temps, Vec::<PathBuf>::new());
    }

    /// Many publishers and readers of one key at once, as threads: every
    /// restore is a miss or the whole object, exactly one publish stores,
    /// and nothing is left behind.
    #[test]
    fn concurrent_publishers_and_readers_agree() {
        let fx = Fixture::new();
        let bytes: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 256) as u8).collect();
        let object = fx.object("big.o", &bytes);
        let stored = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for i in 0..16 {
                let (fx, object, bytes, stored) = (&fx, &object, &bytes, &stored);
                scope.spawn(move || {
                    for round in 0..10 {
                        if (i + round) % 2 == 0 {
                            if fx.publish(object, b"warning\n").unwrap() == Published::Stored {
                                stored.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            }
                        } else {
                            let restored = fx.tmp.path().join(format!("build/r{i}.o"));
                            match fx.store.restore(&fx.key, &restored) {
                                Ok(messages) => {
                                    assert_eq!(fs::read(&restored).unwrap(), *bytes);
                                    assert_eq!(messages.stderr, b"warning\n");
                                }
                                Err(Miss::Absent) => {}
                                Err(miss) => panic!("{miss}"),
                            }
                        }
                    }
                });
            }
        });
        assert_eq!(stored.load(std::sync::atomic::Ordering::SeqCst), 1);
        let entry = fx.entry();
        assert_eq!(fx.leftovers(entry.parent().unwrap(), &[&entry]), Vec::<PathBuf>::new());
        let build = fx.tmp.path().join("build");
        let temps: Vec<PathBuf> = fx.leftovers(&build, &[]).into_iter().filter(|p| p.to_string_lossy().contains(".cactup-")).collect();
        assert_eq!(temps, Vec::<PathBuf>::new());
    }
}
