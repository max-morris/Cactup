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
    /// Directories it was given and ignored as nonexistent. One that
    /// exists later would be searched by a later compile.
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
                    if let Some(dir) = quoted(b"ignoring nonexistent directory ") {
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

/// Where a file was found: in the directory of the file that included it,
/// or at a position in the search list (the quote directories, then the
/// bracket ones), or by its absolute name.
#[derive(Debug, Clone, Copy, PartialEq)]
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
    /// The file the run entered for it, by the name its marker gave; none
    /// if the run found a file it had entered before and skipped it.
    entered: Option<Vec<u8>>,
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

/// What lies in a directory, by name, as one listing of it showed.
type Listing = HashMap<Vec<u8>, Listed>;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Listed {
    File,
    Directory,
    /// A symlink, or anything the listing did not say the kind of: looked
    /// at by its path.
    Other,
}

/// Lookups for one pass over a compile's includes: each directory is
/// listed once, and names in it are answered from the listing (a few
/// listings where looking at every place a compiler tries would take
/// thousands of system calls). Nothing is kept from one pass to the next.
#[derive(Default)]
pub struct Looker {
    listings: HashMap<Vec<u8>, Option<Listing>>,
    /// System calls made: listings and lookups by path.
    pub count: u64,
}

impl Looker {
    /// `spelling` in `dir`: the path, if a file is there (following
    /// symlinks; a directory there is passed over, as the compilers do).
    /// `Err` where GCC would take a precompiled header (`<file>.gch`)
    /// instead, and for what is neither file nor directory, or cannot be
    /// looked at: not modeled.
    fn file(&mut self, dir: &[u8], spelling: &[u8]) -> Result<Option<Vec<u8>>, String> {
        let path = joined(dir, spelling);
        let gch = [spelling, b".gch"].concat();
        if !matches!(self.at(dir, &gch, false)?, Entry::Absent) {
            return Err("a precompiled header is where the compiler looks".to_owned());
        }
        Ok(matches!(self.at(dir, spelling, true)?, Entry::File).then_some(path))
    }

    /// What is at `spelling` in `dir` (following a final symlink if
    /// `follow`), from listings where the names are plain, or by the path.
    fn at(&mut self, dir: &[u8], spelling: &[u8], follow: bool) -> Result<Entry, String> {
        let parts: Vec<&[u8]> = spelling.split(|b| *b == b'/').collect();
        if spelling.starts_with(b"/") || parts.iter().any(|part| matches!(*part, b"" | b"." | b"..")) {
            self.count += 1;
            return entry(&joined(dir, spelling), follow);
        }
        let mut here = if dir.is_empty() { b".".to_vec() } else { dir.to_vec() };
        for (at, part) in parts.iter().enumerate() {
            let last = at + 1 == parts.len();
            let listed = match self.listing(&here) {
                Some(listing) => listing.get(*part).copied(),
                None => Some(Listed::Other),
            };
            let next = joined(&here, part);
            let kind = match listed {
                None => return Ok(Entry::Absent),
                Some(Listed::File) => Entry::File,
                Some(Listed::Directory) => Entry::Directory,
                Some(Listed::Other) => {
                    self.count += 1;
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

    /// The listing of `dir`: empty if there is no such directory, `None` if
    /// it cannot be listed (its names are then looked at by path).
    fn listing(&mut self, dir: &[u8]) -> Option<&Listing> {
        if !self.listings.contains_key(dir) {
            self.count += 1;
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
                    .collect::<Option<Listing>>(),
                Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => Some(Listing::new()),
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
    /// The files being read, innermost last, by the names their entering
    /// markers gave (the first marker names the source).
    stack: Vec<Vec<u8>>,
    /// The name the last marker gave: the compiler's own pseudo-file names
    /// show there (`<command-line>`).
    current: Vec<u8>,
    directives: Vec<Directive>,
    /// The directive printed last, while no entering marker or text has
    /// followed it.
    pending: Option<usize>,
    /// Why the output cannot be followed, if it cannot.
    unmodeled: Option<String>,
}

impl Tracker {
    /// A line marker naming `name`, entering a file (`enters`) or going
    /// back to one (`returns`).
    pub fn marker(&mut self, name: &[u8], enters: bool, returns: bool) {
        if self.stack.is_empty() {
            self.stack.push(name.to_vec());
        }
        if enters {
            match self.pending.take() {
                Some(at) => self.directives[at].entered = Some(name.to_vec()),
                // GCC reads `stdc-predef.h` before the source, unasked, as
                // if `<stdc-predef.h>` were included from the command line.
                None if is_command_line(&self.current) && name.ends_with(b"/stdc-predef.h") => self.directives.push(Directive {
                    kind: Kind::Angled,
                    spelling: b"stdc-predef.h".to_vec(),
                    from: self.current.clone(),
                    entered: Some(name.to_vec()),
                }),
                // Clang enters `<built-in>` and `<command line>` so.
                None if name.starts_with(b"<") && name.ends_with(b">") => {}
                None => self.unmodeled("a file was entered that no #include names"),
            }
            self.stack.push(name.to_vec());
        } else if returns && self.stack.len() > 1 {
            self.stack.pop();
        }
        self.current = name.to_vec();
    }

    /// A `-dI` line.
    pub fn directive(&mut self, kind: Kind, spelling: Vec<u8>) {
        let from = self.stack.last().cloned().unwrap_or_default();
        self.directives.push(Directive { kind, spelling, from, entered: None });
        self.pending = Some(self.directives.len() - 1);
    }

    /// Any other line that is not blank: the directive before it entered
    /// nothing.
    pub fn text(&mut self) {
        self.pending = None;
    }

    fn unmodeled(&mut self, why: &str) {
        self.unmodeled.get_or_insert_with(|| why.to_owned());
    }

    /// What was collected, with GCC's unasked `stdc-predef.h` looked for
    /// where it was not found (it would be read if it appeared).
    pub fn finish(mut self, gcc: bool) -> Result<Vec<Directive>, String> {
        if let Some(why) = self.unmodeled.take() {
            return Err(why);
        }
        if gcc && !self.directives.iter().any(|d| is_command_line(&d.from)) {
            self.directives.insert(0, Directive { kind: Kind::Angled, spelling: b"stdc-predef.h".to_vec(), from: b"<command-line>".to_vec(), entered: None });
        }
        Ok(self.directives)
    }
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

/// Find the `__has_include`s (and `__has_include_next`s) in `bytes`, a file
/// the compile reads. `Err`: one this module does not follow — one whose
/// argument is not a literal name (a macro), or the name used other than
/// to call it or to ask whether it is defined (where a macro can call it
/// in turn); a comment begun on its own line aside.
pub fn probes(bytes: &[u8], out: &mut Vec<Probe>) -> Result<(), String> {
    const NAME: &[u8] = b"__has_include";
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$';
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
            let inner = &bytes[paren + 1..];
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
        // In a comment (`#endif // __has_include`), where nothing is
        // asked.
        if in_comment(bytes, start) {
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

/// Is `at` in `bytes` inside a comment begun on its own line: after a `//`
/// or a `/*` (not closed since) that no string or character literal holds?
/// (A comment begun on an earlier line is not told from a `/*` in a string
/// there: such a `__has_include` is taken for one in code.)
fn in_comment(bytes: &[u8], at: usize) -> bool {
    let line_start = bytes[..at].iter().rposition(|b| *b == b'\n').map_or(0, |at| at + 1);
    let (mut quote, mut block) = (None, false);
    let mut line = bytes[line_start..at].iter().peekable();
    while let Some(b) = line.next() {
        let next = line.peek().copied().copied();
        match (quote, block, *b) {
            (_, true, b'*') if next == Some(b'/') => {
                line.next();
                block = false;
            }
            (_, true, _) => {}
            (Some(_), _, b'\\') => {
                line.next();
            }
            (Some(open), _, b) if b == open => quote = None,
            (Some(_), _, _) => {}
            (None, _, b'"' | b'\'') => quote = Some(*b),
            (None, _, b'/') if next == Some(b'/') => return true,
            (None, _, b'/') if next == Some(b'*') => {
                line.next();
                block = true;
            }
            _ => {}
        }
    }
    block
}

fn trim_end(bytes: &[u8]) -> &[u8] {
    let len = bytes.iter().rposition(|b| !matches!(b, b' ' | b'\t')).map_or(0, |at| at + 1);
    &bytes[..len]
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|window| window == needle)
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
    /// Lookups made, before and after the compile.
    pub count: u64,
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
        Ok(Self { search, directives, probes, dirs, expected: None, count: 0, gcc })
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
        let mut hasher = Hasher::new("has_include");
        let mapped = |name: &[u8]| map.map_or_else(|| name.to_vec(), |map| map.apply(name));
        let mut looker = Looker::default();
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
        self.count += looker.count;
        Ok(hasher.hex())
    }

    /// Look every include up as the compiler did, before the compile; each
    /// must lead where the run went (a skipped one, to a file it had
    /// entered before). `Err`: the search is not modeled here, or the
    /// files changed since the run; either way the check is left to a
    /// second preprocessor run.
    pub fn before_compile(&mut self) -> Result<(), String> {
        let found = self.look_up()?;
        let mut entered = HashSet::new();
        for (directive, found) in self.directives.iter().zip(&found) {
            match (&directive.entered, found) {
                (Some(name), Some(found)) if *name == found.path || canonical(&found.path).is_some_and(|path| path == *name) => {
                    entered.insert(found.path.clone());
                }
                (None, Some(found)) if entered.contains(&found.path) => {}
                (None, None) if is_command_line(&directive.from) => {}
                _ => return Err(format!("the compiler found {} elsewhere than cactup would", String::from_utf8_lossy(&directive.spelling))),
            }
        }
        self.expected = Some(found);
        Ok(())
    }

    /// Were the includes looked up before the compile?
    pub fn looked_up(&self) -> bool {
        self.expected.is_some()
    }

    /// After the compile: does every include still lead where it did, and
    /// is every directory the run ignored as nonexistent still so?
    pub fn still_hold(&mut self) -> bool {
        for dir in &self.search.absent {
            self.count += 1;
            if !matches!(entry(dir, false), Ok(Entry::Absent)) {
                return false;
            }
        }
        self.look_up().is_ok_and(|found| self.expected.as_ref() == Some(&found))
    }

    /// Every include looked up, in the run's order.
    fn look_up(&mut self) -> Result<Vec<Option<Found>>, String> {
        let mut places: HashMap<&[u8], Place> = HashMap::new();
        let mut out = Vec::with_capacity(self.directives.len());
        let mut looker = Looker::default();
        for directive in &self.directives {
            let found = match directive.spelling.starts_with(b"/") {
                true => self.search.find(&directive.spelling, std::iter::once((&b""[..], Place::Absolute)), &mut looker)?,
                false => {
                    let dirs = self.search.dirs(directive.kind, &directive.from, places.get(directive.from.as_slice()).copied(), self.gcc);
                    self.search.find(&directive.spelling, dirs.into_iter(), &mut looker)?
                }
            };
            if let (Some(name), Some(found)) = (&directive.entered, &found) {
                places.insert(name.as_slice(), found.place);
            }
            out.push(found);
        }
        self.count += looker.count;
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

    #[test]
    fn finds_has_include_where_it_can_be_followed() {
        let mut out = Vec::new();
        probes(b"#if __has_include(<tbb/tbb.h>)\n#  define X __has_include( \"y.h\" )\n#endif // __has_include\n#ifdef __has_include\n#if defined(__has_include) && defined __has_include\nint my__has_include;\n/* __has_include */ /* a */\n", &mut out).unwrap();
        assert_eq!(out, vec![Probe { angled: true, spelling: b"tbb/tbb.h".to_vec() }, Probe { angled: false, spelling: b"y.h".to_vec() }]);
        for odd in [&b"#if __has_include(HEADER)\n"[..], b"#define H __has_include\n", b"#if __has_include_next(X)\n", b"#if __has_include(<x.h\n", b"#define H \"//\" __has_include\n", b"/* */ __has_include\n", b"/* a\n __has_include */\n"] {
            assert!(probes(odd, &mut Vec::new()).is_err(), "{}", String::from_utf8_lossy(odd));
        }
    }
}
