//! The C and C++ preprocessor's file search, repeated by cactup (§18.5): what
//! the check after a compile does in place of running the preprocessor a
//! second time.
//!
//! The key's preprocessor run is given `-dI`, so its output has each
//! `#include` and `#include_next` as written (a macro-named one expanded),
//! just before the line marker that enters the file it found; an include
//! skipped because its file was guarded (or `#pragma once`) is printed with
//! no entering marker after it. Its `-v` says where it searched: the quote
//! directories, then the bracket ones, and the directories it ignored as
//! nonexistent. That is everything needed to look each name up again the
//! way the compiler does ([`Search::find`]):
//!
//! - `"name"`: the directory of the including file (the file entered, not
//!   the name a `#line` gave it), then the quote directories, then the
//!   bracket ones; `<name>`: the bracket directories; `#include_next`: the
//!   directories after the one its including file was found in. An
//!   absolute name is not searched for.
//! - A directory where a file could be is passed over (GCC and Clang
//!   alike); a precompiled header (`<name>.gch`) where GCC looks is not
//!   modeled, and neither is anything that is neither file nor directory.
//!
//! `__has_include` leaves no trace in the output, so the answers are worked
//! out here for every literal `__has_include` in the files read
//! ([`Lookups::answers`]) and go into the key: an answer that changed after
//! the run asked would only make a key that no compile with consistent
//! inputs arrives at.
//!
//! Whatever this does not model leaves the check to a second preprocessor
//! run, as before ([`Lookups::unmodeled`]).

use super::key::PathMap;
use super::hash::Hasher;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io::ErrorKind;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

/// Where the preprocessor searched, as its `-v` says.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Search {
    /// The `-iquote` directories, searched for `"name"` only.
    quote: Vec<Vec<u8>>,
    /// The bracket directories (`-I`, `-isystem`, the system ones,
    /// `-idirafter`), searched for both.
    bracket: Vec<Vec<u8>>,
    /// Directories it was given and left out: nonexistent, or not a
    /// directory (GCC warns of that one instead). One that is a directory
    /// later would be searched by a later compile.
    absent: Vec<Vec<u8>>,
}

impl Search {
    /// Read the search lists from what a driver run with `-v` wrote on
    /// stderr (GCC and Clang word it alike). `Err`: they cannot be read as
    /// this module understands them.
    pub fn from_verbose(said: &[u8]) -> Result<Self, String> {
        let mut search = Self::default();
        let mut duplicates = Vec::new();
        // 0: before the lists, 1: quote, 2: bracket, 3: after.
        let mut part = 0;
        for line in said.split(|b| *b == b'\n') {
            let quoted = |prefix: &[u8]| -> Option<Vec<u8>> {
                let rest = line.strip_prefix(prefix)?.strip_prefix(b"\"")?;
                Some(rest.strip_suffix(b"\"")?.to_vec())
            };
            match (part, line) {
                (0, b"#include \"...\" search starts here:") => part = 1,
                (0 | 1, b"#include <...> search starts here:") => part = 2,
                (1 | 2, b"End of search list.") => part = 3,
                (1 | 2, _) if line.starts_with(b" ") => {
                    let dir = line[1..].to_vec();
                    // Clang marks framework directories and header maps,
                    // which are searched otherwise.
                    if dir.ends_with(b")") {
                        return Err("the compiler searches a framework directory or a header map".to_owned());
                    }
                    match part {
                        1 => search.quote.push(dir),
                        _ => search.bracket.push(dir),
                    }
                }
                (1 | 2, _) => return Err("the compiler's search list cannot be read".to_owned()),
                (0, _) => {
                    // `cc1: warning: <dir>: not a directory`.
                    let not_a_dir = find(line, b": warning: ").and_then(|at| line[at + 11..].strip_suffix(b": not a directory"));
                    if let Some(dir) = not_a_dir {
                        search.absent.push(dir.to_vec());
                    } else if let Some(dir) = quoted(b"ignoring nonexistent directory ") {
                        search.absent.push(dir);
                    } else if let Some(dir) = quoted(b"ignoring duplicate directory ") {
                        duplicates.push(dir);
                    }
                }
                _ => {}
            }
        }
        if part != 3 {
            return Err("the compiler did not say where it searches".to_owned());
        }
        // A directory dropped as the duplicate of another by name is the
        // same directory whatever happens; one dropped as the same
        // directory under another name may not stay so.
        if let Some(dir) = duplicates.iter().find(|dir| !search.quote.contains(dir) && !search.bracket.contains(dir)) {
            return Err(format!("the compiler dropped a directory as the same as another ({})", String::from_utf8_lossy(dir)));
        }
        Ok(search)
    }

    /// Look `spelling` up in `dirs` (each with its position, as
    /// [`Found`]), as the compiler does: the first file found.
    fn find<'a>(&self, spelling: &[u8], dirs: impl Iterator<Item = (&'a [u8], Place)>, looker: &mut Looker) -> Result<Option<Found>, String> {
        for (dir, place) in dirs {
            if let Some(found) = looker.file(dir, spelling)? {
                return Ok(Some(Found { path: found, place }));
            }
        }
        Ok(None)
    }

    /// Every directory a lookup of `kind` from `from` (found at
    /// `from_place`; none for the source itself) searches, in order.
    /// `#include_next` goes on after the directory its file was found in;
    /// from a file found otherwise, GCC goes on from the start of the list
    /// when that file was found beside the file including it, and searches
    /// as for a plain include when it was the source itself or named
    /// absolutely, as Clang does from any of them.
    fn dirs<'a>(&'a self, kind: Kind, from: &'a [u8], from_place: Option<Place>, gcc: bool) -> Vec<(&'a [u8], Place)> {
        let chain = self.quote.iter().chain(&self.bracket).enumerate().map(|(at, dir)| (dir.as_slice(), Place::Chain(at)));
        let plain = match kind {
            Kind::Quote | Kind::Next { angled: false } => false,
            Kind::Angled | Kind::Next { angled: true } => true,
        };
        match (kind, from_place) {
            (Kind::Next { .. }, Some(Place::Chain(at))) => chain.skip(at + 1).collect(),
            (Kind::Next { .. }, Some(Place::Including)) if gcc => chain.collect(),
            _ if plain => chain.skip(self.quote.len()).collect(),
            _ => std::iter::once((dir_of(from), Place::Including)).chain(chain).collect(),
        }
    }
}

/// One search: the directories in order, and the name.
type Lookup<'a> = (Vec<(&'a [u8], Place)>, &'a [u8]);

/// Where a file was found: in the directory of the file that included it,
/// or at a position in the search list (the quote directories, then the
/// bracket ones), or by its absolute name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Place {
    Including,
    Chain(usize),
    Absolute,
}

#[derive(Debug, Clone, PartialEq)]
struct Found {
    path: Vec<u8>,
    place: Place,
}

/// How an include names its file: `"name"`, `<name>`, `#include_next`
/// (with either).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Quote,
    Angled,
    Next { angled: bool },
}

/// One `#include` the key's preprocessor run carried out.
#[derive(Debug, Clone, PartialEq)]
pub struct Directive {
    kind: Kind,
    spelling: Vec<u8>,
    /// The file it stands in, as the run entered it.
    from: Vec<u8>,
    /// The include that entered that file (none for the source itself):
    /// where it was found is where `#include_next` goes on from.
    by: Option<usize>,
    /// The file the run entered for it, by the name its marker gave; none
    /// if the run found a file it had entered before and skipped it.
    entered: Option<Vec<u8>>,
    /// The entering marker said the file is a system header: GCC may then
    /// have named it by its physical path.
    system: bool,
}

/// What lies at a path, as far as the search is concerned.
enum Entry {
    Absent,
    File,
    Directory,
}

/// What is at `path` (following symlinks if `follow`). Anything but a
/// plain answer is not modeled.
fn entry(path: &[u8], follow: bool) -> Result<Entry, String> {
    let path = Path::new(OsStr::from_bytes(path));
    let meta = match follow {
        true => std::fs::metadata(path),
        false => std::fs::symlink_metadata(path),
    };
    match meta {
        Ok(meta) if meta.is_file() => Ok(Entry::File),
        Ok(meta) if meta.is_dir() => Ok(Entry::Directory),
        Ok(_) if !follow => Ok(Entry::File),
        Ok(_) => Err(format!("{} is neither a file nor a directory", path.display())),
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => Ok(Entry::Absent),
        Err(e) => Err(format!("{} cannot be looked at ({e})", path.display())),
    }
}

/// What lies in a directory, by name, as one listing of it showed; and
/// whether any name in it ends in `.gch` (if none does, no precompiled
/// header needs looking for there).
#[derive(Debug, Default)]
struct Listing {
    names: HashMap<Vec<u8>, Listed>,
    gch: bool,
    /// The names in lower case (ASCII), and whether any is not ASCII: in a
    /// directory that folds case (ext4's `casefold`), a name the listing
    /// lacks may still be found under another case, so it is then looked
    /// at by its path.
    folded: HashSet<Vec<u8>>,
    unfolded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Listed {
    File,
    Directory,
    /// A symlink, or anything the listing did not say the kind of: looked
    /// at by its path.
    Other,
}

/// Lookups for one pass over a compile's includes: a directory looked in
/// more than a few times is listed once, and further names in it are
/// answered from the listing (a few listings where looking at every place
/// a compiler tries would take thousands of system calls; a large
/// directory looked in once or twice is not listed). The key's answers and
/// the pass before the compile share one; the check after the compile has
/// its own.
#[derive(Debug, Default)]
pub struct Looker {
    listings: HashMap<Vec<u8>, Option<Listing>>,
    /// Names looked at by path in each directory not listed yet.
    looks: HashMap<Vec<u8>, u32>,
    /// Lookups by path, and directories listed.
    stats: u64,
    listed: u64,
}

impl Looker {
    /// `spelling` in `dir`: the path, if a file is there (following
    /// symlinks; a directory there is passed over, as the compilers do).
    /// `Err` where GCC would take a precompiled header (`<file>.gch`)
    /// instead, and for what is neither file nor directory, or cannot be
    /// looked at: not modeled.
    fn file(&mut self, dir: &[u8], spelling: &[u8]) -> Result<Option<Vec<u8>>, String> {
        let gch = [spelling, b".gch"].concat();
        if !matches!(self.at(dir, &gch, false)?, Entry::Absent) {
            return Err("a precompiled header is where the compiler looks".to_owned());
        }
        Ok(matches!(self.at(dir, spelling, true)?, Entry::File).then(|| joined(dir, spelling)))
    }

    /// What is at `spelling` in `dir` (following a final symlink if
    /// `follow`), from listings where the names are plain, or by the path.
    fn at(&mut self, dir: &[u8], spelling: &[u8], follow: bool) -> Result<Entry, String> {
        let parts: Vec<&[u8]> = spelling.split(|b| *b == b'/').collect();
        if spelling.starts_with(b"/") || parts.iter().any(|part| matches!(*part, b"" | b"." | b"..")) {
            self.stats += 1;
            return entry(&joined(dir, spelling), follow);
        }
        let mut here = if dir.is_empty() { b".".to_vec() } else { dir.to_vec() };
        for (at, part) in parts.iter().enumerate() {
            let last = at + 1 == parts.len();
            // A `.gch` name, in a listed directory with none.
            if last && part.ends_with(b".gch") && self.listings.get(here.as_slice()).is_some_and(|listing| listing.as_ref().is_some_and(|listing| !listing.gch)) {
                return Ok(Entry::Absent);
            }
            let next = joined(&here, part);
            let listed = self.listed(&here, part, &next)?;
            let kind = match listed {
                None => return Ok(Entry::Absent),
                Some(Listed::File) => Entry::File,
                Some(Listed::Directory) => Entry::Directory,
                Some(Listed::Other) => {
                    self.stats += 1;
                    entry(&next, follow || !last)?
                }
            };
            match (last, kind) {
                (true, kind) => return Ok(kind),
                (false, Entry::Directory) => here = next,
                (false, _) => return Ok(Entry::Absent),
            }
        }
        Ok(Entry::Absent)
    }

    /// What `name` is in `dir` (`path` is the two joined): from the listing
    /// once `dir` has been looked in often enough to list it, by the path
    /// (not following a symlink) until then. `None`: nothing of that name.
    fn listed(&mut self, dir: &[u8], name: &[u8], path: &[u8]) -> Result<Option<Listed>, String> {
        const BEFORE_LISTING: u32 = 4;
        let looks = match self.looks.get_mut(dir) {
            Some(looks) => looks,
            None => self.looks.entry(dir.to_vec()).or_default(),
        };
        let by_path = *looks < BEFORE_LISTING && !self.listings.contains_key(dir);
        if by_path {
            *looks += 1;
        } else {
            match self.listing(dir) {
                Some(listing) => match listing.names.get(name) {
                    Some(listed) => return Ok(Some(*listed)),
                    None if listing.unfolded || !name.is_ascii() || listing.folded.contains(&name.to_ascii_lowercase()) => {}
                    None => return Ok(None),
                },
                None => return Ok(Some(Listed::Other)),
            }
        }
        self.stats += 1;
        match std::fs::symlink_metadata(Path::new(OsStr::from_bytes(path))) {
            Ok(meta) if meta.is_file() => Ok(Some(Listed::File)),
            Ok(meta) if meta.is_dir() => Ok(Some(Listed::Directory)),
            Ok(_) => Ok(Some(Listed::Other)),
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => Ok(None),
            Err(e) => Err(format!("{} cannot be looked at ({e})", String::from_utf8_lossy(path))),
        }
    }

    /// The listing of `dir`: empty if there is no such directory, `None` if
    /// it cannot be listed (its names are then looked at by path).
    fn listing(&mut self, dir: &[u8]) -> Option<&Listing> {
        if !self.listings.contains_key(dir) {
            self.listed += 1;
            let listing = match std::fs::read_dir(Path::new(OsStr::from_bytes(dir))) {
                Ok(entries) => entries
                    .map(|entry| {
                        let entry = entry.ok()?;
                        let kind = match entry.file_type().ok()? {
                            kind if kind.is_file() => Listed::File,
                            kind if kind.is_dir() => Listed::Directory,
                            _ => Listed::Other,
                        };
                        Some((entry.file_name().into_vec(), kind))
                    })
                    .collect::<Option<HashMap<_, _>>>()
                    .map(|names| Listing {
                        gch: names.keys().any(|name| name.ends_with(b".gch")),
                        folded: names.keys().map(|name| name.to_ascii_lowercase()).collect(),
                        unfolded: names.keys().any(|name| !name.is_ascii()),
                        names,
                    }),
                Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => Some(Listing::default()),
                Err(_) => None,
            };
            self.listings.insert(dir.to_vec(), listing);
        }
        self.listings.get(dir).and_then(Option::as_ref)
    }
}

/// The physical path of `path`: GCC names a system header so where that is
/// shorter (`-fcanonical-system-headers`, its default).
fn canonical(path: &[u8]) -> Option<Vec<u8>> {
    std::fs::canonicalize(Path::new(OsStr::from_bytes(path))).ok().map(|path| path.into_os_string().into_vec())
}

/// `spelling` in `dir`, as the compilers put them together: nothing in
/// between when `dir` is empty or ends in `/`, a `/` otherwise; an absolute
/// `spelling` alone.
fn joined(dir: &[u8], spelling: &[u8]) -> Vec<u8> {
    match () {
        _ if spelling.starts_with(b"/") || dir.is_empty() => spelling.to_vec(),
        _ if dir.ends_with(b"/") => [dir, spelling].concat(),
        _ => [dir, b"/", spelling].concat(),
    }
}

/// The directory part of `file`, with its `/` (GCC's own rule: empty for a
/// name without one).
fn dir_of(file: &[u8]) -> &[u8] {
    file.iter().rposition(|b| *b == b'/').map_or(&[][..], |at| &file[..=at])
}

/// Take apart a `-dI` line: `#include <x>`, `#include "x"`,
/// `#include_next …` (Clang adds ` /* clang -E -dI */`). `None`: not such a
/// line.
pub fn directive(line: &[u8]) -> Option<(Kind, Vec<u8>)> {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let line = line.strip_suffix(b" /* clang -E -dI */").unwrap_or(line);
    let (next, rest) = match (line.strip_prefix(b"#include_next "), line.strip_prefix(b"#include ")) {
        (Some(rest), _) => (true, rest),
        (None, Some(rest)) => (false, rest),
        _ => return None,
    };
    let (angled, close) = match rest.first()? {
        b'<' => (true, b'>'),
        b'"' => (false, b'"'),
        _ => return None,
    };
    let spelling = rest[1..].strip_suffix(&[close])?;
    if spelling.contains(&close) || spelling.is_empty() {
        return None;
    }
    let kind = match (next, angled) {
        (true, angled) => Kind::Next { angled },
        (false, true) => Kind::Angled,
        (false, false) => Kind::Quote,
    };
    Some((kind, spelling.to_vec()))
}

/// The includes of a preprocessor run's output, collected as it is read:
/// fed each line marker and each `-dI` line, and told of every other line
/// that is not blank.
#[derive(Debug, Default)]
pub struct Tracker {
    /// The files being read, innermost last (the first marker names the
    /// source).
    stack: Vec<Frame>,
    directives: Vec<Directive>,
    /// The directive printed last, while no entering marker or text has
    /// followed it.
    pending: Option<usize>,
    /// Why the output cannot be followed, if it cannot.
    unmodeled: Option<String>,
}

/// A file being read, in a run's output.
#[derive(Debug)]
struct Frame {
    /// The name its entering marker gave.
    entered: Vec<u8>,
    /// The name its last marker gave (a `#line` changes it, and so do the
    /// compilers' own pseudo-files, `<command-line>`): a marker that returns
    /// to it gives this name.
    shown: Vec<u8>,
    /// The include that entered it.
    by: Option<usize>,
}

impl Tracker {
    /// A line marker naming `name`, entering a file (`enters`) or going
    /// back to one (`returns`). A marker that returns elsewhere than to the
    /// file being read before (a line in a raw string, or a `#` line in a
    /// file, that looks like one) is not followed.
    pub fn marker(&mut self, name: &[u8], enters: bool, returns: bool, system: bool) {
        let Some(top) = self.stack.last_mut() else {
            self.stack.push(Frame { entered: name.to_vec(), shown: name.to_vec(), by: None });
            return;
        };
        if enters {
            let shown = top.shown.clone();
            let by = match self.pending.take() {
                Some(at) => {
                    self.directives[at].entered = Some(name.to_vec());
                    self.directives[at].system = system;
                    Some(at)
                }
                // GCC reads `stdc-predef.h` before the source, unasked, as
                // if `<stdc-predef.h>` were included from the command line.
                None if is_command_line(&shown) && name.ends_with(b"/stdc-predef.h") => {
                    self.directives.push(Directive {
                        kind: Kind::Angled,
                        spelling: b"stdc-predef.h".to_vec(),
                        from: shown,
                        by: None,
                        entered: Some(name.to_vec()),
                        system,
                    });
                    Some(self.directives.len() - 1)
                }
                // Clang enters `<built-in>` and `<command line>` so.
                None if name.starts_with(b"<") && name.ends_with(b">") => None,
                None => {
                    self.unmodeled("a file was entered that no #include names");
                    None
                }
            };
            self.stack.push(Frame { entered: name.to_vec(), shown: name.to_vec(), by });
        } else if returns {
            self.stack.pop();
            match self.stack.last_mut() {
                Some(top) if top.shown == name => {}
                _ => self.unmodeled("a line marker returns to a file the output was not reading"),
            }
        } else {
            top.shown = name.to_vec();
        }
    }

    /// A `-dI` line.
    pub fn directive(&mut self, kind: Kind, spelling: Vec<u8>) {
        let (from, by) = self.stack.last().map_or((Vec::new(), None), |top| (top.entered.clone(), top.by));
        self.directives.push(Directive { kind, spelling, from, by, entered: None, system: false });
        self.pending = Some(self.directives.len() - 1);
    }

    /// A line of another directive `-dI` prints that this module does not
    /// follow (`#import`).
    pub fn unfollowed(&mut self) {
        self.unmodeled("a source has #import, which the cache does not follow");
    }

    /// Any other line that is not blank: the directive before it entered
    /// nothing.
    pub fn text(&mut self) {
        self.pending = None;
    }

    fn unmodeled(&mut self, why: &str) {
        self.unmodeled.get_or_insert_with(|| why.to_owned());
    }

    /// What was collected, with `stdc-predef.h` looked for where GCC reads
    /// it unasked (`preinclude`) and it was not found: it would be read if
    /// it appeared.
    pub fn finish(mut self, preinclude: bool) -> Result<Vec<Directive>, String> {
        if let Some(why) = self.unmodeled.take() {
            return Err(why);
        }
        if preinclude && !self.directives.iter().any(|d| is_command_line(&d.from)) {
            self.directives.push(Directive {
                kind: Kind::Angled,
                spelling: b"stdc-predef.h".to_vec(),
                from: b"<command-line>".to_vec(),
                by: None,
                entered: None,
                system: false,
            });
        }
        Ok(self.directives)
    }
}

/// Is `line` a directive `-dI` prints that is not an include this module
/// follows?
pub fn unfollowed_directive(line: &[u8]) -> bool {
    line.starts_with(b"#import ") || line.starts_with(b"#__include_macros ")
}

fn is_command_line(name: &[u8]) -> bool {
    name == b"<command-line>" || name == b"<command line>"
}

/// A `__has_include` (or `__has_include_next`) whose argument is a literal
/// name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Probe {
    angled: bool,
    spelling: Vec<u8>,
}

/// What the scan of the files a compile reads found, all files together:
/// the `__has_include`s.
#[derive(Debug, Default)]
pub struct Scan {
    pub probes: Vec<Probe>,
}

/// Scan `bytes`, a file the compile reads (or a macro given on the command
/// line), for what the check by lookups must know of or cannot follow. It
/// is read as the compilers read it — lines spliced by a backslash joined,
/// comments taken out, literals kept apart — and wherever GCC and Clang, or
/// one language mode and another, could read it otherwise, the scan stops
/// with `Err`, and the check is left to a second compiler run:
///
/// - a NUL byte, a carriage return that ends a line by itself, a trigraph
///   (`??` and one of `=/'()!<>-`), a raw string (`R"…"`), a number with a
///   `'` in it (a digit separator, or not, by the language);
/// - `#embed` and `__has_embed`, which read a file no line marker names;
/// - `__TIMESTAMP__`, and `__DATE__` or `__TIME__` unless `dated` (the
///   compile's `SOURCE_DATE_EPOCH` fixes them): the second compiler run saw
///   a clock that moved on;
/// - a line marker given in the source (`# 12 "file" 2`), and a `#` that
///   is no directive and no operator of a function-like macro (a macro can
///   put one at the start of an output line): either can write a line the
///   reader of the output takes for the compiler's own marker;
/// - a `__has_include` (or `__has_include_next`) whose argument is not a
///   name as written, or that is used other than to call it or to ask
///   whether it is defined (a macro can call it in turn).
///
/// A name pasted together from pieces (`__has_ ## include`) is not seen:
/// a stated limit (decision 15).
pub fn scan(bytes: &[u8], dated: bool, scan: &mut Scan) -> Result<(), String> {
    let refuse = |why: &str| Err(format!("a source has {why}, which the cache does not follow"));
    if memchr::memchr(0, bytes).is_some() {
        return refuse("a NUL byte");
    }
    if memchr::memchr_iter(b'\r', bytes).any(|at| bytes.get(at + 1) != Some(&b'\n')) {
        return refuse("a carriage return that ends a line by itself");
    }
    let trigraph = memchr::memmem::find_iter(bytes, b"??").any(|at| bytes.get(at + 2).is_some_and(|b| b"=/'()!<>-".contains(b)));
    if trigraph {
        return refuse("a trigraph");
    }
    let spliced = spliced(bytes);
    if !worth_reading(&spliced) {
        return Ok(());
    }
    let (code, bare) = code(&spliced)?;
    // Words outside literals.
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$';
    let word = |word: &[u8]| {
        memchr::memmem::find_iter(&bare, word).any(|at| {
            let joined_before = at > 0 && ident(bare[at - 1]);
            let joined_after = bare.get(at + word.len()).is_some_and(|b| ident(*b));
            !joined_before && !joined_after
        })
    };
    if word(b"__has_embed") {
        return refuse("__has_embed");
    }
    if word(b"__TIMESTAMP__") {
        return refuse("__TIMESTAMP__");
    }
    if !dated && (word(b"__DATE__") || word(b"__TIME__")) {
        return refuse("the date or time of the compile");
    }
    // Each line with a `#` in it (or `%:`, or the word `embed`).
    let mut lines: Vec<usize> = memchr::memchr_iter(b'#', &bare)
        .chain(memchr::memmem::find_iter(&bare, b"%:"))
        .chain(memchr::memmem::find_iter(&bare, b"embed"))
        .map(|at| bare[..at].iter().rposition(|b| *b == b'\n').map_or(0, |at| at + 1))
        .collect();
    lines.sort_unstable();
    lines.dedup();
    for start in lines {
        let end = memchr::memchr(b'\n', &bare[start..]).map_or(bare.len(), |len| start + len);
        directive_line(&bare[start..end])?;
    }
    probes(&code, &bare, &mut scan.probes)
}

/// Does `bytes` (lines spliced) have anything the scan looks for, read in
/// any way at all: a watched name or what could begin one (`__has_`,
/// `__DATE__`, `__TIME`), `embed`, the digraph `%:`, a `#` that does not
/// begin its line, or a line that begins with `#` and a digit? Most files
/// have none, and need no reading as code.
fn worth_reading(bytes: &[u8]) -> bool {
    let words = [&b"__has_"[..], b"__DATE__", b"__TIME", b"embed", b"%:"];
    if words.iter().any(|word| memchr::memmem::find(bytes, word).is_some()) {
        return true;
    }
    let blank = |b: &u8| matches!(b, b' ' | b'\t' | b'\x0b' | b'\x0c' | b'\r');
    memchr::memchr_iter(b'#', bytes).any(|at| {
        let line_start = bytes[..at].iter().rposition(|b| *b == b'\n').map_or(0, |at| at + 1);
        let after = &bytes[at + 1..];
        let after = &after[after.iter().take_while(|b| blank(b)).count()..];
        !bytes[line_start..at].iter().all(blank) || after.first().is_some_and(u8::is_ascii_digit)
    })
}

/// Check one line of a file's code (comments out, literals blanked) for a
/// `#` the scan does not follow ([`scan`]).
fn directive_line(line: &[u8]) -> Result<(), String> {
    let blank = |b: &u8| matches!(b, b' ' | b'\t' | b'\x0b' | b'\x0c' | b'\r');
    let ident = |b: &u8| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'$';
    let hash = |text: &[u8]| find(text, b"#").is_some() || find(text, b"%:").is_some();
    let refuse = |why: &str| Err(format!("a source has {why}, which the cache does not follow"));
    let line = &line[line.iter().take_while(|b| blank(b)).count()..];
    let Some(rest) = line.strip_prefix(b"#").or_else(|| line.strip_prefix(b"%:")) else {
        return match hash(line) {
            true => refuse("a # outside a directive"),
            false => Ok(()),
        };
    };
    let rest = &rest[rest.iter().take_while(|b| blank(b)).count()..];
    if rest.first().is_some_and(u8::is_ascii_digit) {
        return refuse("a line marker of its own");
    }
    let name_len = rest.iter().take_while(|b| ident(b)).count();
    match &rest[..name_len] {
        b"embed" => refuse("#embed"),
        b"define" => {
            let after = &rest[name_len..];
            let after = &after[after.iter().take_while(|b| blank(b)).count()..];
            let macro_len = after.iter().take_while(|b| ident(b)).count();
            // An object-like macro: its `#` is a token of its expansion.
            match after.get(macro_len) == Some(&b'(') || !hash(&after[macro_len..]) {
                true => Ok(()),
                false => refuse("a # in an object-like macro"),
            }
        }
        _ => Ok(()),
    }
}

/// `bytes` with every line splice taken out: a backslash, blanks, and a
/// line end (GCC and Clang allow the blanks).
fn spliced(bytes: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    let splice_at = |at: usize| -> Option<usize> {
        let rest = &bytes[at + 1..];
        let blanks = rest.iter().take_while(|b| matches!(b, b' ' | b'\t' | b'\x0b' | b'\x0c')).count();
        match &rest[blanks..] {
            [b'\n', ..] => Some(1 + blanks + 1),
            [b'\r', b'\n', ..] => Some(1 + blanks + 2),
            _ => None,
        }
    };
    let mut out: Option<Vec<u8>> = None;
    let mut copied = 0;
    let mut at = 0;
    while let Some(found) = memchr::memchr(b'\\', &bytes[at..]) {
        let slash = at + found;
        at = slash + 1;
        if let Some(len) = splice_at(slash) {
            let out = out.get_or_insert_with(|| Vec::with_capacity(bytes.len()));
            out.extend_from_slice(&bytes[copied..slash]);
            copied = slash + len;
            at = copied;
        }
    }
    match out {
        Some(mut out) => {
            out.extend_from_slice(&bytes[copied..]);
            std::borrow::Cow::Owned(out)
        }
        None => std::borrow::Cow::Borrowed(bytes),
    }
}

/// A file's code, its lines spliced already: each comment one blank, as the
/// compilers take it, literals kept (`code`); and the same with the inside
/// of every literal blanked (`bare`, as long). `Err` for what reads
/// otherwise by compiler or language: a raw string, a number with a `'`.
fn code(bytes: &[u8]) -> Result<(Vec<u8>, Vec<u8>), String> {
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$';
    let refuse = |why: &str| Err(format!("a source has {why}, which the cache does not follow"));
    let (mut code, mut bare) = (Vec::with_capacity(bytes.len()), Vec::with_capacity(bytes.len()));
    let mut at = 0;
    // From one byte that matters to the next (`/`, a quote), what lies
    // between is code as it is.
    while let Some(found) = memchr::memchr3(b'/', b'"', b'\'', &bytes[at..]) {
        let here = at + found;
        code.extend_from_slice(&bytes[at..here]);
        bare.extend_from_slice(&bytes[at..here]);
        at = here;
        let next = bytes.get(at + 1).copied();
        match bytes[at] {
            b'/' if next == Some(b'/') => {
                at = memchr::memchr(b'\n', &bytes[at..]).map_or(bytes.len(), |len| at + len);
                code.push(b' ');
                bare.push(b' ');
            }
            b'/' if next == Some(b'*') => {
                at = find(&bytes[at + 2..], b"*/").map_or(bytes.len(), |len| at + 2 + len + 2);
                code.push(b' ');
                bare.push(b' ');
            }
            b'/' => {
                code.push(b'/');
                bare.push(b'/');
                at += 1;
            }
            quote => {
                // The token the quote follows: an identifier that makes it a
                // raw string (`R"`), or a number, in which a `'` is a digit
                // separator in C++14 and C23 and begins a character literal
                // before them.
                let mut start = at;
                while start > 0 {
                    let b = bytes[start - 1];
                    let sign = matches!(b, b'+' | b'-') && start > 1 && matches!(bytes[start - 2], b'e' | b'E' | b'p' | b'P');
                    if !(ident(b) || b == b'.' || sign) {
                        break;
                    }
                    start -= 1;
                }
                let token = &bytes[start..at];
                let number = token.first().is_some_and(u8::is_ascii_digit) || (token.first() == Some(&b'.') && token.get(1).is_some_and(u8::is_ascii_digit));
                if quote == b'\'' && number {
                    return refuse("a ' in a number");
                }
                let prefix_start = bytes[..at].iter().rposition(|b| !ident(*b)).map_or(0, |p| p + 1);
                if quote == b'"' && matches!(&bytes[prefix_start..at], b"R" | b"LR" | b"uR" | b"UR" | b"u8R") {
                    return refuse("a raw string");
                }
                // To the closing quote, or the end of the line.
                let begin = at;
                at += 1;
                loop {
                    let Some(found) = memchr::memchr3(quote, b'\n', b'\\', &bytes[at.min(bytes.len())..]) else {
                        at = bytes.len();
                        break;
                    };
                    at += found;
                    match bytes[at] {
                        b'\\' => at += 2,
                        b'\n' => break,
                        _ => {
                            at += 1;
                            break;
                        }
                    }
                }
                at = at.min(bytes.len());
                code.extend_from_slice(&bytes[begin..at]);
                bare.push(quote);
                bare.extend(std::iter::repeat_n(b' ', at - begin - 1));
            }
        }
    }
    code.extend_from_slice(&bytes[at..]);
    bare.extend_from_slice(&bytes[at..]);
    Ok((code, bare))
}

/// Find the `__has_include`s (and `__has_include_next`s) in `code`, with
/// `bare` the same with its literals blanked, as [`scan`] says.
fn probes(code: &[u8], bare: &[u8], out: &mut Vec<Probe>) -> Result<(), String> {
    const NAME: &[u8] = b"__has_include";
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$';
    // Looked for where no literal is; read where literals are kept (a
    // `"name"` is one).
    let bytes = bare;
    let mut at = 0;
    while let Some(found) = find(&bytes[at..], NAME) {
        let start = at + found;
        let mut end = start + NAME.len();
        at = end;
        if start > 0 && ident(bytes[start - 1]) {
            continue;
        }
        let rest = &bytes[end..];
        // `__has_include_next` is answered as `__has_include` is (each
        // directory), and GCC before 10 also knew `__has_include__`.
        if rest.starts_with(b"_next") && !rest.get(5).is_some_and(|b| ident(*b)) {
            end += 5;
        } else if rest.starts_with(b"__") && !rest.get(2).is_some_and(|b| ident(*b)) {
            end += 2;
        } else if rest.first().is_some_and(|b| ident(*b)) {
            continue;
        }
        let blank = |b: &u8| matches!(b, b' ' | b'\t');
        let paren = end + bytes[end..].iter().take_while(|b| blank(b)).count();
        if bytes.get(paren) == Some(&b'(') {
            let inner = &code[paren + 1..];
            let inner = &inner[inner.iter().take_while(|b| blank(b)).count()..];
            let (angled, close) = match inner.first() {
                Some(b'<') => (true, b'>'),
                Some(b'"') => (false, b'"'),
                _ => return Err("a source asks __has_include of something other than a name as written".to_owned()),
            };
            let len = inner[1..].iter().position(|b| *b == close || *b == b'\n');
            let unreadable = || "a __has_include cannot be read".to_owned();
            let len = len.filter(|len| *len > 0 && inner[1 + len] == close).ok_or_else(unreadable)?;
            let tail = &inner[2 + len..];
            if tail.iter().find(|b| !blank(b)) != Some(&b')') {
                return Err(unreadable());
            }
            out.push(Probe { angled, spelling: inner[1..1 + len].to_vec() });
            continue;
        }
        // Only asked whether it is defined: `defined __has_include`,
        // `defined(__has_include)`, `#ifdef __has_include`.
        let before = trim_end(&bytes[..start]);
        let before = before.strip_suffix(b"(").map_or(before, trim_end);
        let line_start = before.iter().rposition(|b| *b == b'\n').map_or(0, |at| at + 1);
        let words: Vec<&[u8]> = before[line_start..].split(|b| !ident(*b) && *b != b'#').filter(|w| !w.is_empty()).collect();
        let defined = before.ends_with(b"defined")
            && !before.get(before.len().wrapping_sub(8)).is_some_and(|b| ident(*b))
            || matches!(words.as_slice(), [b"#ifdef"] | [b"#ifndef"] | [b"#", b"ifdef"] | [b"#", b"ifndef"]);
        if !defined {
            return Err("a source uses __has_include other than to call it, which the cache does not follow".to_owned());
        }
    }
    Ok(())
}

fn trim_end(bytes: &[u8]) -> &[u8] {
    let len = bytes.iter().rposition(|b| !matches!(b, b' ' | b'\t')).map_or(0, |at| at + 1);
    &bytes[..len]
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    memchr::memmem::find(hay, needle)
}

/// What the check after a C or C++ compile looks up instead of running the
/// preprocessor again: the run's search lists, its includes, and the
/// `__has_include`s of the files it read.
#[derive(Debug)]
pub struct Lookups {
    search: Search,
    directives: Vec<Directive>,
    probes: Vec<Probe>,
    /// The directories of the files read: a `"name"` asked of
    /// `__has_include` (from a macro, maybe, used in any of them) looks
    /// there first.
    dirs: Vec<Vec<u8>>,
    /// Where each include led, as cactup found it before the compile
    /// ([`Lookups::before_compile`]); none until then.
    expected: Option<Vec<Option<Found>>>,
    /// For each include whose file GCC named by its physical path, that
    /// name: after the compile, the file found must still be it.
    physical: Vec<(usize, Vec<u8>)>,
    /// The lookups of the key's answers, which the pass before the compile
    /// goes on with (the directories as listed then are as good as any
    /// taken after the key's run).
    looker: Option<Looker>,
    /// Lookups by path, and directories listed, before and after the
    /// compile.
    pub stats: u64,
    pub listed: u64,
    /// GCC, which goes on with `#include_next` otherwise than Clang.
    gcc: bool,
}

impl Lookups {
    /// The lookups of a run whose `-v` said `said`, whose output had
    /// `directives`, and whose files (`files`, by the names the markers
    /// gave) asked `probes`.
    pub fn new(said: &[u8], directives: Vec<Directive>, mut probes: Vec<Probe>, files: &[Vec<u8>], gcc: bool) -> Result<Self, String> {
        let search = Search::from_verbose(said)?;
        probes.sort();
        probes.dedup();
        let mut dirs: Vec<Vec<u8>> = files.iter().map(|file| dir_of(file).to_vec()).collect();
        dirs.sort();
        dirs.dedup();
        Ok(Self { search, directives, probes, dirs, expected: None, physical: Vec::new(), looker: None, stats: 0, listed: 0, gcc })
    }

    /// The answers to the `__has_include`s, digested for the key, names
    /// under `map`.
    ///
    /// Each name is answered for every directory it could be looked for
    /// in: each of the search list, and the directory of each file read
    /// (a `"name"` is looked for beside the file that asks, which a macro
    /// can make any of them; `__has_include_next` goes on from where its
    /// file was found). Whatever the compiler asked, its answer follows from
    /// these.
    pub fn answers(&mut self, map: Option<&PathMap>) -> Result<String, String> {
        let mut looker = Looker::default();
        let answers = self.answer(map, &mut looker);
        self.looker = Some(looker);
        answers
    }

    fn answer(&self, map: Option<&PathMap>, looker: &mut Looker) -> Result<String, String> {
        let mut hasher = Hasher::new("has_include");
        let mapped = |name: &[u8]| map.map_or_else(|| name.to_vec(), |map| map.apply(name));
        // The search list itself, which the positions below are in: the
        // answers are a function of it, and the key has the `-I` flags only
        // through the text.
        if !self.probes.is_empty() {
            for dir in self.search.quote.iter().chain(&self.search.bracket) {
                hasher.feed(&mapped(dir));
            }
            hasher.feed(&(self.search.quote.len() as u64).to_le_bytes());
        }
        for probe in &self.probes {
            hasher.feed(&mapped(&probe.spelling));
            for (at, dir) in self.search.quote.iter().chain(&self.search.bracket).enumerate() {
                if looker.file(dir, &probe.spelling)?.is_some() {
                    hasher.feed(&(at as u64).to_le_bytes());
                }
            }
            hasher.feed(b"beside");
            if !probe.angled {
                for dir in &self.dirs {
                    if looker.file(dir, &probe.spelling)?.is_some() {
                        hasher.feed(&mapped(dir));
                    }
                }
            }
        }
        Ok(hasher.hex())
    }

    /// Count `looker`'s work into the check's.
    fn counted(&mut self, looker: &Looker) {
        self.stats += looker.stats;
        self.listed += looker.listed;
    }

    /// Look every include up as the compiler did, before the compile; each
    /// must lead where the run went (a skipped one, to a file it had
    /// entered before). `Err`: the search is not modeled here, or the
    /// files changed since the run; either way the check is left to a
    /// second preprocessor run.
    pub fn before_compile(&mut self) -> Result<(), String> {
        let mut looker = self.looker.take().unwrap_or_default();
        let (stats, listed) = (looker.stats, looker.listed);
        let found = self.look_up(&mut looker);
        self.stats += looker.stats - stats;
        self.listed += looker.listed - listed;
        let found = found?;
        let mut entered = HashSet::new();
        let mut physical = Vec::new();
        // GCC names a system header by its physical path where that is
        // shorter than the path it was found by
        // (`-fcanonical-system-headers`, its default).
        let gcc = self.gcc;
        let shorter = |directive: &Directive, name: &[u8], path: &[u8]| {
            gcc && directive.system && name.len() < path.len() && canonical(path).is_some_and(|path| path == name)
        };
        for (at, (directive, found)) in self.directives.iter().zip(&found).enumerate() {
            match (&directive.entered, found) {
                (Some(name), Some(found)) if *name == found.path => {
                    entered.insert(found.path.clone());
                }
                (Some(name), Some(found)) if shorter(directive, name, &found.path) => {
                    entered.insert(found.path.clone());
                    physical.push((at, name.clone()));
                }
                (None, Some(found)) if entered.contains(&found.path) => {}
                (None, None) if is_command_line(&directive.from) => {}
                _ => return Err(format!("the compiler found {} elsewhere than cactup would", String::from_utf8_lossy(&directive.spelling))),
            }
        }
        self.expected = Some(found);
        self.physical = physical;
        Ok(())
    }

    /// Were the includes looked up before the compile?
    pub fn looked_up(&self) -> bool {
        self.expected.is_some()
    }

    /// After the compile: does every include still lead where it did, and
    /// is every directory the run left out still not a directory? If so,
    /// the answers to the `__has_include`s now, digested as for the key.
    pub fn again(&mut self, map: Option<&PathMap>) -> Option<String> {
        let mut looker = Looker::default();
        let answers = self.hold_again(map, &mut looker);
        self.counted(&looker);
        answers
    }

    fn hold_again(&self, map: Option<&PathMap>, looker: &mut Looker) -> Option<String> {
        for dir in &self.search.absent {
            looker.stats += 1;
            match std::fs::metadata(Path::new(OsStr::from_bytes(dir))) {
                Ok(meta) if !meta.is_dir() => {}
                Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {}
                _ => return None,
            }
        }
        let found = self.look_up(looker).ok()?;
        (self.expected.as_ref() == Some(&found)).then_some(())?;
        // A file named by its physical path must still be that file: a
        // symlink on the way to it may have been turned elsewhere.
        for (at, name) in &self.physical {
            looker.stats += 1;
            let path = &found[*at].as_ref()?.path;
            (canonical(path).as_deref() == Some(name.as_slice())).then_some(())?;
        }
        self.answer(map, looker).ok()
    }

    /// Every include looked up, in the run's order.
    fn look_up(&self, looker: &mut Looker) -> Result<Vec<Option<Found>>, String> {
        let mut places: Vec<Option<Place>> = Vec::with_capacity(self.directives.len());
        let mut out = Vec::with_capacity(self.directives.len());
        // The same name searched for along the same directories, as many
        // includes are, is found where it was found before in this pass.
        let mut found_before: HashMap<Lookup, Option<Found>> = HashMap::new();
        for directive in &self.directives {
            let found = match directive.spelling.starts_with(b"/") {
                true => self.search.find(&directive.spelling, std::iter::once((&b""[..], Place::Absolute)), looker)?,
                false => {
                    let dirs = self.search.dirs(directive.kind, &directive.from, directive.by.and_then(|by| places[by]), self.gcc);
                    let memo = (dirs, directive.spelling.as_slice());
                    match found_before.get(&memo) {
                        Some(found) => found.clone(),
                        None => {
                            let found = self.search.find(&directive.spelling, memo.0.iter().copied(), looker)?;
                            found_before.insert(memo, found.clone());
                            found
                        }
                    }
                }
            };
            places.push(found.as_ref().filter(|_| directive.entered.is_some()).map(|found| found.place));
            out.push(found);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_search_lists_gcc_prints() {
        let said = b"Using built-in specs.\nignoring nonexistent directory \"/usr/local/include/x86_64-linux-gnu\"\nignoring duplicate directory \"inc\"\n#include \"...\" search starts here:\n q\n#include <...> search starts here:\n inc\n /usr/include\nEnd of search list.\n";
        let search = Search::from_verbose(said).unwrap();
        assert_eq!(search.quote, vec![b"q".to_vec()]);
        assert_eq!(search.bracket, vec![b"inc".to_vec(), b"/usr/include".to_vec()]);
        assert_eq!(search.absent, vec![b"/usr/local/include/x86_64-linux-gnu".to_vec()]);
        // A directory dropped as another by another name.
        let said = b"ignoring duplicate directory \"other\"\n#include \"...\" search starts here:\n#include <...> search starts here:\n inc\nEnd of search list.\n";
        assert!(Search::from_verbose(said).is_err());
        assert!(Search::from_verbose(b"nothing\n").is_err());
        let framework = b"#include \"...\" search starts here:\n#include <...> search starts here:\n /Library/Frameworks (framework directory)\nEnd of search list.\n";
        assert!(Search::from_verbose(framework).is_err());
    }

    #[test]
    fn reads_include_lines_of_both_compilers() {
        assert_eq!(directive(b"#include <a/b.h>\n"), Some((Kind::Angled, b"a/b.h".to_vec())));
        assert_eq!(directive(b"#include \"x.h\" /* clang -E -dI */\n"), Some((Kind::Quote, b"x.h".to_vec())));
        assert_eq!(directive(b"#include_next <x.h>"), Some((Kind::Next { angled: true }, b"x.h".to_vec())));
        assert_eq!(directive(b"# include <x.h>"), None);
        assert_eq!(directive(b"#include <x.h> trailing"), None);
        assert_eq!(directive(b"#include <>"), None);
    }

    #[test]
    fn names_are_put_together_as_the_compilers_do() {
        assert_eq!(joined(b"", b"a.h"), b"a.h");
        assert_eq!(joined(b"inc/", b"a.h"), b"inc/a.h");
        assert_eq!(joined(b"inc", b"sub/a.h"), b"inc/sub/a.h");
        assert_eq!(joined(b"inc", b"/abs/a.h"), b"/abs/a.h");
        assert_eq!(dir_of(b"s.c"), b"");
        assert_eq!(dir_of(b"inc/sub/b.h"), b"inc/sub/");
    }

    fn scanned(bytes: &[u8], dated: bool) -> Result<Vec<Probe>, String> {
        let mut scan = Scan::default();
        super::scan(bytes, dated, &mut scan)?;
        Ok(scan.probes)
    }

    /// The scan reads a file as the compilers do, and what they could read
    /// otherwise goes to the second compiler run.
    #[test]
    fn the_scan_reads_as_the_compilers_do() {
        let spliced = scanned(b"#if __has_\\\ninclude(<x.h>)\n#endif\n", false).unwrap();
        assert_eq!(spliced, vec![Probe { angled: true, spelling: b"x.h".to_vec() }]);
        assert!(scanned(b"#if __has_\\  \r\ninclude(<x.h>)\r\n", false).is_ok_and(|probes| probes.len() == 1));
        for odd in [
            // Read otherwise by one compiler or language than another.
            &b"#if __has_\\\rinclude(<x.h>)\n"[..],
            b"// x\r#define H __has_include\n",
            b"int x;\0\n",
            b"#if __has_include(<x.h>) ??/\n#endif\n",
            b"#define CAT(a, b) a ??=??= b\n",
            b"const char *s = R\"x(\n# 1 \"s.c\" 2\n)x\";\n",
            b"#define W 1'a/*'\n#define H __has_include\n",
            // What the scan leaves to the compiler.
            b"#embed \"data.bin\"\n",
            b"  %: embed <data.bin>\n",
            b"#/* the data */ embed \"data.bin\"\n",
            b"/* the data: */ #embed \"data.bin\"\n",
            b"#if __has_embed(\"data.bin\")\n#endif\n",
            b"const char *when = __DATE__ \" \" __TIME__;\n",
            b"const char *when = __TIMESTAMP__;\n",
            b"int g(int, const char *, int);\nint x = g(1'0, \"x'/*\", 1);\n#define H __has_include\n",
            // What could write a line taken for a line marker.
            b"# 1 \"s.c\" 2\n",
            b"#define M # 1 \"s.c\" 2\nM\n",
            b"#define E(x) x\nE(#) 1 \"s.c\" 2\n",
        ] {
            assert!(scanned(odd, false).is_err(), "{}", String::from_utf8_lossy(odd));
        }
        for fine in [
            // Comments and literals are not code.
            &b"/* __DATE__ #embed */ const char *s = \"__TIME__ # 1\"; // __has_embed\n"[..],
            b"#define STR(x) #x\n#define CAT(a, b) a ## b\n#line 7 \"orig.c\"\n",
            b"int auR = 1; const char *s = auR\"x\";\n",
            b"int embedded;\n#define embed 1\n",
            // A name pasted from pieces is a stated limit (decision 15).
            b"#define HAS(h) __has_ ## include(h)\n#if HAS(<x.h>)\n#endif\n",
        ] {
            assert!(scanned(fine, false).is_ok(), "{}: {:?}", String::from_utf8_lossy(fine), scanned(fine, false));
        }
        assert!(scanned(b"const char *when = __DATE__;\n", true).is_ok(), "SOURCE_DATE_EPOCH fixes the date");
    }

    /// A line marker that returns elsewhere than to the file being read,
    /// and an `#import`, leave the output unfollowed.
    #[test]
    fn the_tracker_follows_only_what_it_understands() {
        let mut tracker = Tracker::default();
        tracker.marker(b"s.c", false, false, false);
        tracker.directive(Kind::Quote, b"a.h".to_vec());
        tracker.marker(b"a.h", true, false, false);
        tracker.directive(Kind::Quote, b"b.h".to_vec());
        tracker.marker(b"b.h", true, false, false);
        tracker.marker(b"a.h", false, true, false);
        tracker.marker(b"s.c", false, true, false);
        let directives = tracker.finish(false).unwrap();
        assert_eq!(directives[1].from, b"a.h");
        assert_eq!(directives[1].by, Some(0));
        let mut fake = Tracker::default();
        fake.marker(b"s.c", false, false, false);
        fake.directive(Kind::Quote, b"a.h".to_vec());
        fake.marker(b"a.h", true, false, false);
        fake.marker(b"s.c", false, true, false);
        fake.marker(b"s.c", false, true, false);
        assert!(fake.finish(false).is_err(), "a second return from the source");
        let mut imports = Tracker::default();
        imports.marker(b"s.c", false, false, false);
        assert!(unfollowed_directive(b"#import \"y.h\"\n"));
        imports.unfollowed();
        assert!(imports.finish(false).is_err());
    }

    #[test]
    fn finds_has_include_where_it_can_be_followed() {
        let out = scanned(b"#if __has_include(<tbb/tbb.h>)\n#  define X __has_include( \"y.h\" )\n#endif // __has_include\n#ifdef __has_include\n#if defined(__has_include) && defined __has_include\nint my__has_include;\n/* __has_include */ /* a */\n/* two\n   lines: __has_include argument */\n// spliced \\\n__has_include\nconst char *s = \"\\\"\"; // __has_include\n", false).unwrap();
        assert_eq!(out, vec![Probe { angled: true, spelling: b"tbb/tbb.h".to_vec() }, Probe { angled: false, spelling: b"y.h".to_vec() }]);
        for odd in [&b"#if __has_include(HEADER)\n"[..], b"#define H __has_include\n", b"#if __has_include_next(X)\n", b"#if __has_include(<x.h\n", b"#define H \"//\" __has_include\n", b"/* */ __has_include\n", b"#define H \"/*\" __has_include\n", b"R\"x(/*)x\" __has_include\n"] {
            assert!(scanned(odd, false).is_err(), "{}", String::from_utf8_lossy(odd));
        }
    }
}
