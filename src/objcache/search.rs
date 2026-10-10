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

use super::key::{self, PathMap};
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
                        gch: names.keys().any(|name| name.to_ascii_lowercase().ends_with(b".gch")),
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

/// What the path `path` resolves through (`key::trail`: each directory and
/// symlink, by identity), digested: a symlink turned elsewhere and back is a
/// new one.
fn resolved(path: &[u8]) -> String {
    let mut hasher = Hasher::new("resolved");
    key::trail(Path::new(OsStr::from_bytes(path)), &mut hasher, &mut HashMap::new());
    hasher.hex()
}

/// How many entries the path `path` names (each one a lookup to resolve).
fn path_entries(path: &[u8]) -> u64 {
    Path::new(OsStr::from_bytes(path)).components().count() as u64
}

/// The directories a compile names for its search, from its arguments:
/// all of them, and those given by `-I` or `-iquote` (no system ones).
#[derive(Debug, Default)]
pub struct Given {
    all: Vec<Vec<u8>>,
    user: Vec<Vec<u8>>,
    /// A flag that makes directories this does not work out (`-iprefix`
    /// and its `-iwithprefix…`, a sysroot).
    unfollowed: Option<String>,
}

impl Given {
    /// From a compile's arguments, and the variables GCC and Clang read for
    /// it: `CPATH` (as `-I`), and `C_INCLUDE_PATH` or `CPLUS_INCLUDE_PATH`
    /// by the language (as `-isystem`), where an empty entry is the working
    /// directory.
    pub fn from_args(args: &[std::ffi::OsString], cxx: bool) -> Self {
        let mut given = Self::default();
        let mut args = args.iter().map(|arg| arg.as_bytes());
        while let Some(arg) = args.next() {
            for flag in [&b"-iprefix"[..], b"-iwithprefix", b"-isysroot", b"--sysroot"] {
                if arg.starts_with(flag) {
                    given.unfollowed = Some(format!("{} is given", String::from_utf8_lossy(flag)));
                }
            }
            for (flag, user) in [(&b"-I"[..], true), (b"-iquote", true), (b"-isystem", false), (b"-idirafter", false)] {
                let Some(joined) = arg.strip_prefix(flag) else { continue };
                let dir = match joined.is_empty() {
                    true => args.next().unwrap_or_default(),
                    false => joined,
                };
                given.add(dir, user);
                break;
            }
        }
        let language = if cxx { "CPLUS_INCLUDE_PATH" } else { "C_INCLUDE_PATH" };
        for (variable, user) in [("CPATH", true), (language, false)] {
            if let Some(value) = std::env::var_os(variable) {
                for dir in value.as_bytes().split(|b| *b == b':') {
                    given.add(if dir.is_empty() { b"." } else { dir }, user);
                }
            }
        }
        given
    }

    fn add(&mut self, dir: &[u8], user: bool) {
        self.all.push(dir.to_vec());
        if user {
            self.user.push(dir.to_vec());
        }
    }

    /// Is `listed`, as a search list prints it, the directory `dir` given
    /// (with or without a trailing `/`)?
    fn same(dir: &[u8], listed: &[u8]) -> bool {
        let bare = |name: &[u8]| -> Vec<u8> {
            let len = name.iter().rposition(|b| *b != b'/').map_or(name.len().min(1), |at| at + 1);
            name[..len].to_vec()
        };
        bare(dir) == bare(listed)
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
    /// Every name `-dD` showed defined or undefined (the compilers' own
    /// among them), and the `__has_include`s in macros.
    defined: HashSet<Vec<u8>>,
    macro_probes: Vec<Probe>,
}

/// What a run's output showed, for [`Lookups::new`].
#[derive(Debug)]
pub struct Followed {
    directives: Vec<Directive>,
    defined: HashSet<Vec<u8>>,
    probes: Vec<Probe>,
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

    /// A line `-dD` printed: `#define …` or `#undef …`. `false` if it is
    /// neither.
    pub fn macro_line(&mut self, line: &[u8]) -> bool {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        let (rest, define) = match (line.strip_prefix(b"#define "), line.strip_prefix(b"#undef ")) {
            (Some(rest), _) => (rest, true),
            (None, Some(rest)) => (rest, false),
            _ => return false,
        };
        self.text();
        let name_len = rest.iter().take_while(|b| ident(**b)).count();
        self.defined.insert(rest[..name_len].to_vec());
        if define && let Err(why) = macro_body(rest, &mut self.macro_probes) {
            self.unmodeled(&why);
        }
        true
    }

    fn unmodeled(&mut self, why: &str) {
        self.unmodeled.get_or_insert_with(|| why.to_owned());
    }

    /// What was collected, with `stdc-predef.h` looked for where GCC reads
    /// it unasked (`preinclude`) and it was not found: it would be read if
    /// it appeared.
    pub fn finish(mut self, preinclude: bool) -> Result<Followed, String> {
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
        Ok(Followed { directives: self.directives, defined: self.defined, probes: self.macro_probes })
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
    /// A `<name>` the compilers may expand macros in: one not written
    /// right on an `#if` or `#elif` line (in a macro, or in a macro's
    /// argument), where `<`, the words and `>` are tokens already.
    expanded: bool,
}

/// What the scan of the files a compile reads found, all files together:
/// the `__has_include`s.
#[derive(Debug, Default)]
pub struct Scan {
    pub probes: Vec<Probe>,
}

/// Scan `bytes`, a file the compile reads (or a macro given on the command
/// line), for what the check by lookups must know of or cannot follow.
///
/// The scan does not tell code from comments and literals: what it looks
/// for counts wherever it stands, so that no misreading of a file can hide
/// anything from it (a `__has_include` in a comment only makes one lookup
/// more). The one exception is a comment it can prove from its own line.
/// Lines spliced by a backslash are joined first. `Err` — the check is
/// left to a second compiler run — for
///
/// - a NUL byte, a carriage return that ends a line by itself, a trigraph
///   (`??` and one of `=/'()!<>-`): read otherwise by one compiler or
///   language mode than another;
/// - `__has_embed`, `__TIMESTAMP__`, `__DATE__` and `__TIME__` anywhere,
///   and an `#embed` directive: a file no line marker names, and a clock
///   that moved on between the key's run and the compile;
/// - a line marker given in a source (`# 12 "file" 2`), and a `#` that
///   follows `(` or `,` outside a directive (a `#` passed to a macro can
///   start an output line): either can write a line the reader of the
///   output takes for the compiler's own marker;
/// - a `__has_include` (or `__has_include_next`) whose argument is not a
///   name as written, or that is used other than to call it or to ask
///   whether it is defined (a macro can call it in turn), unless its line
///   shows it to be in a comment.
///
/// What macros make is judged from the run's own `-dD` output instead
/// ([`macro_body`]); a name pasted from pieces is a stated limit (decision
/// 15).
pub fn scan(bytes: &[u8], scan: &mut Scan) -> Result<(), String> {
    let refuse = |why: &str| Err(format!("a source has {why}, which the cache does not follow"));
    if memchr::memchr(0, bytes).is_some() {
        return refuse("a NUL byte");
    }
    if memchr::memchr_iter(b'\r', bytes).any(|at| bytes.get(at + 1) != Some(&b'\n')) {
        return refuse("a carriage return that ends a line by itself");
    }
    if memchr::memmem::find_iter(bytes, b"??").any(|at| bytes.get(at + 2).is_some_and(|b| b"=/'()!<>-".contains(b))) {
        return refuse("a trigraph");
    }
    let bytes = &spliced(bytes)[..];
    // A byte order mark the compilers pass over at the start of a file.
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    for word in [&b"__has_embed"[..], b"__TIMESTAMP__", b"__DATE__", b"__TIME__"] {
        if words(bytes, word).next().is_some() {
            return refuse(&format!("{}", String::from_utf8_lossy(word)));
        }
    }
    let hashes = memchr::memchr_iter(b'#', bytes).chain(memchr::memmem::find_iter(bytes, b"%:"));
    for at in hashes {
        let len = if bytes[at] == b'#' { 1 } else { 2 };
        if begins_line(bytes, at) {
            let after = &bytes[skip_blanks(bytes, at + len)..];
            if after.first().is_some_and(u8::is_ascii_digit) {
                return refuse("a line marker of its own");
            }
            if after.strip_prefix(b"embed").is_some_and(|rest| !rest.first().is_some_and(|b| ident(*b))) {
                return refuse("#embed");
            }
        } else if !directive_line(bytes, at) && matches!(before_blanks(bytes, at), Some(b'(' | b',')) && marker_follows(bytes, at + len) {
            return refuse("a # passed to a macro, with what a line marker has after it");
        }
    }
    probes(bytes, &mut scan.probes)
}

/// Does what follows `at`, on its line or the next (a macro's call can
/// span them), have what a line marker has after its `#`: a number, blanks,
/// a quote?
fn marker_follows(bytes: &[u8], at: usize) -> bool {
    let line_end = |from: usize| memchr::memchr(b'\n', &bytes[from..]).map_or(bytes.len(), |len| from + len);
    let end = line_end(line_end(at).saturating_add(1).min(bytes.len()));
    let text = &bytes[at..end];
    text.iter().enumerate().any(|(i, b)| {
        b.is_ascii_digit() && !(i > 0 && ident(text[i - 1])) && {
            let digits = text[i..].iter().take_while(|b| b.is_ascii_digit()).count();
            let blanks = text[i + digits..].iter().take_while(|b| matches!(b, b' ' | b'\t')).count();
            blanks > 0 && text.get(i + digits + blanks) == Some(&b'"')
        }
    })
}

/// Can `b` be part of an identifier?
fn ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// Where `word` stands in `bytes` as a whole identifier.
fn words<'a>(bytes: &'a [u8], word: &'a [u8]) -> impl Iterator<Item = usize> + 'a {
    memchr::memmem::find_iter(bytes, word).filter(move |at| {
        let joined_before = *at > 0 && ident(bytes[at - 1]);
        let joined_after = bytes.get(at + word.len()).is_some_and(|b| ident(*b));
        !joined_before && !joined_after
    })
}

/// The first position at or after `at` past blanks and whole `/* … */`
/// comments (a directive's name may follow its `#` so).
fn skip_blanks(bytes: &[u8], mut at: usize) -> usize {
    loop {
        match bytes.get(at..) {
            Some([b' ' | b'\t' | b'\x0b' | b'\x0c' | b'\r', ..]) => at += 1,
            Some([b'/', b'*', ..]) => match find(&bytes[at + 2..], b"*/") {
                Some(len) => at += 2 + len + 2,
                None => return bytes.len(),
            },
            _ => return at,
        }
    }
}

/// The byte before `at`, past blanks, line ends and whole `/* … */`
/// comments.
fn before_blanks(bytes: &[u8], mut at: usize) -> Option<u8> {
    loop {
        match &bytes[..at] {
            [.., b' ' | b'\t' | b'\x0b' | b'\x0c' | b'\r' | b'\n'] => at -= 1,
            [.., b'*', b'/'] => at = bytes[..at - 2].windows(2).rposition(|w| w == b"/*")?,
            [.., b] => return Some(*b),
            [] => return None,
        }
    }
}

/// Could the `#` (or `%:`) at `at` begin its line, as a directive does:
/// nothing before it on the line but blanks, whole comments, and the end of
/// a comment begun on a line before?
fn begins_line(bytes: &[u8], at: usize) -> bool {
    let line_start = bytes[..at].iter().rposition(|b| *b == b'\n').map_or(0, |at| at + 1);
    let mut end = at;
    loop {
        match &bytes[line_start..end] {
            [] => return true,
            [.., b' ' | b'\t' | b'\x0b' | b'\x0c' | b'\r'] => end -= 1,
            [.., b'*', b'/'] => match bytes[line_start..end - 2].windows(2).rposition(|w| w == b"/*") {
                Some(open) => end = line_start + open,
                None => return true,
            },
            _ => return false,
        }
    }
}

/// Is the line of `at` a directive (its first token a `#` or `%:`)?
fn directive_line(bytes: &[u8], at: usize) -> bool {
    let line_start = bytes[..at].iter().rposition(|b| *b == b'\n').map_or(0, |at| at + 1);
    let first = skip_blanks(bytes, line_start);
    first < at && (bytes[first] == b'#' || bytes[first..].starts_with(b"%:")) && begins_line(bytes, first)
}

/// Is `at` after a `//` on its line that no quote stands before: in a
/// comment (or in a block comment or a raw string the line is inside, where
/// it is no code either)?
fn after_line_comment(bytes: &[u8], at: usize) -> bool {
    let line_start = bytes[..at].iter().rposition(|b| *b == b'\n').map_or(0, |at| at + 1);
    let before = &bytes[line_start..at];
    find(before, b"//").is_some_and(|slashes| !named(&before[..slashes]) && find(&before[slashes..], b"*/").is_none())
}

/// Could `text` hold a literal or a header name, inside which `//`, `/*`
/// and `*/` are no comment marks: a quote, or a `<`?
fn named(text: &[u8]) -> bool {
    text.iter().any(|b| matches!(b, b'"' | b'\'' | b'<' | b'>'))
}

/// Is `[start, end)` shown by its own line to be inside a comment: after a
/// `//` (see [`after_line_comment`]); after a `/*` not closed since; or
/// before the `*/` that ends a block comment begun on a line before (no
/// `/*` before it on the line, nothing between that opens a comment). No
/// quote or header name may stand where it could hide a comment mark.
fn in_comment(bytes: &[u8], start: usize, end: usize) -> bool {
    let line_start = bytes[..start].iter().rposition(|b| *b == b'\n').map_or(0, |at| at + 1);
    let line_end = memchr::memchr(b'\n', &bytes[end..]).map_or(bytes.len(), |len| end + len);
    // Comment marks that share a character (`/**/*`, `*/**/`, `/**//`) read
    // one way from code and another from inside a comment: no proof.
    let line = &bytes[line_start..line_end];
    if [&b"/*/"[..], b"*/*", b"*//"].iter().any(|overlap| find(line, overlap).is_some()) {
        return false;
    }
    let after = &bytes[end..line_end];
    let before = &bytes[line_start..start];
    // After a `/*` not closed since, with no quote or header name before it
    // on the line (inside a block comment either way).
    let opened = before.windows(2).rposition(|w| w == b"/*").is_some_and(|open| !named(before) && find(&before[open..], b"*/").is_none());
    let closed = find(after, b"*/").is_some_and(|close| {
        let between = &after[..close];
        !named(between) && find(between, b"/*").is_none() && find(between, b"//").is_none()
    });
    after_line_comment(bytes, start) || opened || (find(before, b"/*").is_none() && !named(before) && closed)
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

/// The `__has_include`s (and `__has_include_next`s) in `bytes` (lines
/// spliced), as [`scan`] says.
fn probes(bytes: &[u8], out: &mut Vec<Probe>) -> Result<(), String> {
    for (start, end) in has_includes(bytes) {
        let paren = skip_blanks(bytes, end);
        if bytes.get(paren) == Some(&b'(') {
            match literal_name(&bytes[paren + 1..]) {
                Some((angled, spelling)) => {
                    out.push(Probe { angled, spelling, expanded: angled && !written_on_if_line(bytes, start) });
                    continue;
                }
                None if in_comment(bytes, start, end) => continue,
                None => return Err("a source asks __has_include of something other than a name as written".to_owned()),
            }
        }
        // Only asked whether it is defined: `defined __has_include`,
        // `defined(__has_include)`, `#ifdef __has_include`.
        let before = trim_end(&bytes[..start]);
        let before = before.strip_suffix(b"(").map_or(before, trim_end);
        let line_start = before.iter().rposition(|b| *b == b'\n').map_or(0, |at| at + 1);
        let words: Vec<&[u8]> = before[line_start..].split(|b| !ident(*b) && *b != b'#').filter(|w| !w.is_empty()).collect();
        let defined = before.ends_with(b"defined") && !before.get(before.len().wrapping_sub(8)).is_some_and(|b| ident(*b))
            || matches!(words.as_slice(), [b"#ifdef"] | [b"#ifndef"] | [b"#", b"ifdef"] | [b"#", b"ifndef"]);
        if !defined && !in_comment(bytes, start, end) {
            return Err("a source uses __has_include other than to call it, which the cache does not follow".to_owned());
        }
    }
    Ok(())
}

/// Each `__has_include` in `bytes` as a whole word (with `_next`, and GCC
/// before 10's `__has_include__` and `__has_include_next__`), as `[start,
/// end)`.
fn has_includes(bytes: &[u8]) -> impl Iterator<Item = (usize, usize)> + '_ {
    memchr::memmem::find_iter(bytes, b"__has_include").filter_map(move |start| {
        if start > 0 && ident(bytes[start - 1]) {
            return None;
        }
        let mut end = start + b"__has_include".len();
        if bytes[end..].starts_with(b"_next") {
            end += 5;
        }
        if bytes[end..].starts_with(b"__") {
            end += 2;
        }
        (!bytes.get(end).is_some_and(|b| ident(*b))).then_some((start, end))
    })
}

/// The name written as `<name>` or `"name"` (blanks around it), then `)`,
/// at the start of `inner`: whether angled, and the name.
fn literal_name(inner: &[u8]) -> Option<(bool, Vec<u8>)> {
    let blank = |b: &u8| matches!(b, b' ' | b'\t');
    let inner = &inner[inner.iter().take_while(|b| blank(b)).count()..];
    let (angled, close) = match inner.first()? {
        b'<' => (true, b'>'),
        b'"' => (false, b'"'),
        _ => return None,
    };
    let len = inner[1..].iter().position(|b| *b == close || *b == b'\n')?;
    // Blanks at either end are kept by one compiler's reading and not the
    // other's.
    if len == 0 || inner[1 + len] != close || blank(&inner[1]) || blank(&inner[len]) {
        return None;
    }
    let tail = &inner[2 + len..];
    (tail.iter().find(|b| !blank(b)) == Some(&b')')).then(|| (angled, inner[1..1 + len].to_vec()))
}

/// Is the `__has_include` at `at` written right on an `#if` or `#elif`
/// line, not inside a macro's parentheses there? Its `<name>` is then a
/// header name, read as it stands.
fn written_on_if_line(bytes: &[u8], at: usize) -> bool {
    let line_start = bytes[..at].iter().rposition(|b| *b == b'\n').map_or(0, |at| at + 1);
    let first = skip_blanks(bytes, line_start);
    let hash = if bytes[first..].starts_with(b"#") { 1 } else if bytes[first..].starts_with(b"%:") { 2 } else { return false };
    let name = skip_blanks(bytes, first + hash);
    let word_len = bytes[name..].iter().take_while(|b| ident(**b)).count();
    if !matches!(&bytes[name..name + word_len], b"if" | b"elif") || name + word_len > at {
        return false;
    }
    // Before it on the line, no word a macro could be (one could open a
    // call around it, or make a `(` before it), and no comment where one
    // could hide: only `defined`, numbers, operators, and `__has_include`s
    // with the `<name>`s they ask of.
    let text = &bytes[name + word_len..at];
    if find(text, b"/*").is_some() || find(text, b"//").is_some() {
        return false;
    }
    let mut i = 0;
    let mut last_word: &[u8] = b"";
    while i < text.len() {
        match text[i] {
            b if ident(b) => {
                let len = text[i..].iter().take_while(|b| ident(**b)).count();
                let word = &text[i..i + len];
                // `defined X` and `defined(X)` do not expand `X`.
                let fine = word[0].is_ascii_digit() || word == b"defined" || word.starts_with(b"__has_include") || last_word == b"defined";
                if !fine {
                    return false;
                }
                last_word = word;
                i += len;
            }
            b'<' if last_word.starts_with(b"__has_include") && trim_end(&text[..i]).ends_with(b"(") => {
                let Some(close) = text[i..].iter().position(|b| *b == b'>') else { return false };
                last_word = b"";
                i += close + 1;
            }
            b' ' | b'\t' | b'(' => i += 1,
            _ => {
                last_word = b"";
                i += 1;
            }
        }
    }
    true
}

/// Check what `-dD` printed of one macro, `#define <rest>`: the run's own
/// reading of the source, so exact. `Err` for an object-like macro with a
/// `#` in it (it can start an output line with one), and for one that uses
/// a watched name otherwise than to call `__has_include` on a name as
/// written. The probes it makes are angled ones the compilers expand.
pub fn macro_body(rest: &[u8], out: &mut Vec<Probe>) -> Result<(), String> {
    let refuse = |why: &str| Err(format!("a macro has {why}, which the cache does not follow"));
    let name_len = rest.iter().take_while(|b| ident(**b)).count();
    let function_like = rest.get(name_len) == Some(&b'(');
    let body = match function_like {
        true => rest.iter().position(|b| *b == b')').map_or(&rest[..0], |close| &rest[close + 1..]),
        false => &rest[name_len..],
    };
    if !function_like && (memchr::memchr(b'#', body).is_some() || find(body, b"%:").is_some()) {
        return refuse("a #");
    }
    for word in [&b"__has_embed"[..], b"__TIMESTAMP__", b"__DATE__", b"__TIME__"] {
        if words(body, word).next().is_some() {
            return refuse(&format!("{}", String::from_utf8_lossy(word)));
        }
    }
    // A function-like macro's parameters, which its call replaces.
    let params: Vec<&[u8]> = match function_like {
        true => rest[name_len + 1..rest.len() - body.len()]
            .split(|b| !ident(*b))
            .filter(|word| !word.is_empty())
            .chain([&b"__VA_ARGS__"[..], b"__VA_OPT__"])
            .collect(),
        false => Vec::new(),
    };
    for (_, end) in has_includes(body) {
        let paren = skip_blanks(body, end);
        match (body.get(paren), body.get(paren + 1..).and_then(literal_name)) {
            (Some(b'('), Some((true, spelling))) if spelling.split(|b| !ident(*b)).any(|word| params.contains(&word)) => {
                return refuse("__has_include of a name made of its parameters");
            }
            (Some(b'('), Some((angled, spelling))) => out.push(Probe { angled, spelling, expanded: angled }),
            _ => return refuse("__has_include used otherwise than on a name as written"),
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
    /// name and what its path resolved through: after the compile, the file
    /// found must still be it, reached the same way.
    physical: Vec<(usize, Vec<u8>, String)>,
    /// Which directories of the search list (quote, then bracket) are
    /// system ones, where GCC names a header by its physical path.
    system: Vec<bool>,
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
    pub fn new(said: &[u8], followed: Followed, mut probes: Vec<Probe>, files: &[Vec<u8>], gcc: bool, given: &Given) -> Result<Self, String> {
        let mut search = Search::from_verbose(said)?;
        if let Some(why) = &given.unfollowed {
            return Err(why.clone());
        }
        // What the compile was given and the run left out: nonexistent, or
        // no directory (GCC says so in a warning that flags can hide). Each
        // must stay no directory. One that is a directory now was searched
        // under another name.
        for dir in &given.all {
            let listed = |list: &Vec<Vec<u8>>| list.iter().any(|listed| Given::same(dir, listed));
            if listed(&search.quote) || listed(&search.bracket) || listed(&search.absent) {
                continue;
            }
            match std::fs::metadata(Path::new(OsStr::from_bytes(dir))) {
                Ok(meta) if meta.is_dir() => return Err(format!("the compiler searches {} under another name", String::from_utf8_lossy(dir))),
                _ => search.absent.push(dir.clone()),
            }
        }
        // A system directory: a bracket one not given by `-I`.
        let system = search.quote.iter().map(|_| false).chain(search.bracket.iter().map(|dir| !given.user.iter().any(|user| Given::same(user, dir)))).collect();
        probes.extend(followed.probes);
        probes.sort();
        probes.dedup();
        // A `<name>` the compilers expand macros in names another header
        // if a word in it is a macro.
        // The compilers' own dynamic macros (`__LINE__`, `__COUNTER__`,
        // `__FILE__`, …) and `__VA_ARGS__` are no `-dD` names: a word
        // written so is taken for a macro.
        // Reserved words (`__x`, `_X`): the compilers' own macros, built-in
        // function-like ones too (`__has_attribute`), which `-dD` does not
        // show.
        let dynamic = |word: &[u8]| word.starts_with(b"__") || (word.first() == Some(&b'_') && word.get(1).is_some_and(u8::is_ascii_uppercase));
        for probe in probes.iter().filter(|probe| probe.expanded) {
            if probe.spelling.split(|b| !ident(*b)).any(|word| followed.defined.contains(word) || dynamic(word)) {
                return Err(format!("a __has_include in a macro asks for <{}>, a name with a macro in it", String::from_utf8_lossy(&probe.spelling)));
            }
        }
        let mut dirs: Vec<Vec<u8>> = files.iter().map(|file| dir_of(file).to_vec()).collect();
        dirs.sort();
        dirs.dedup();
        let directives = followed.directives;
        Ok(Self { search, directives, probes, dirs, system, expected: None, physical: Vec::new(), looker: None, stats: 0, listed: 0, gcc })
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
        let system = &self.system;
        let shorter = |directive: &Directive, name: &[u8], found: &Found| {
            let in_system = matches!(found.place, Place::Chain(at) if system[at]);
            gcc && directive.system && in_system && name.len() < found.path.len() && canonical(&found.path).is_some_and(|path| path == name)
        };
        for (at, (directive, found)) in self.directives.iter().zip(&found).enumerate() {
            match (&directive.entered, found) {
                (Some(name), Some(found)) if *name == found.path => {
                    entered.insert(found.path.clone());
                }
                (Some(name), Some(found)) if shorter(directive, name, found) => {
                    entered.insert(found.path.clone());
                    physical.push((at, name.clone(), resolved(&found.path)));
                }
                (None, Some(found)) if entered.contains(&found.path) => {}
                (None, None) if is_command_line(&directive.from) => {}
                _ => return Err(format!("the compiler found {} elsewhere than cactup would", String::from_utf8_lossy(&directive.spelling))),
            }
        }
        // The physical paths worked out, and each path resolved through
        // (one lookup per entry on the way).
        self.stats += physical.iter().map(|(at, _, _)| 1 + found[*at].as_ref().map_or(0, |found| path_entries(&found.path))).sum::<u64>();
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
        for (at, name, through) in &self.physical {
            let path = &found[*at].as_ref()?.path;
            looker.stats += 1 + path_entries(path);
            (canonical(path).as_deref() == Some(name.as_slice()) && resolved(path) == *through).then_some(())?;
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

    fn scanned(bytes: &[u8]) -> Result<Vec<Probe>, String> {
        let mut scan = Scan::default();
        super::scan(bytes, &mut scan)?;
        Ok(scan.probes)
    }

    /// The scan counts what it looks for wherever it stands, so no reading
    /// of comments or literals can hide it; what could be read otherwise, or
    /// could forge a line marker, goes to the second compiler run.
    #[test]
    fn the_scan_cannot_be_misled() {
        let probes = |bytes: &[u8]| scanned(bytes).unwrap().into_iter().map(|probe| probe.spelling).collect::<Vec<_>>();
        assert_eq!(probes(b"#if __has_\\\ninclude(<x.h>)\n#endif\n"), [b"x.h".to_vec()]);
        assert_eq!(probes(b"#if __has_\\  \r\ninclude(<x.h>)\r\n"), [b"x.h".to_vec()]);
        // Whatever a reader of comments and literals would make of them.
        assert_eq!(probes(b"#if (e+1'2) && __has_include(<y.h>)\n"), [b"y.h".to_vec()]);
        assert_eq!(probes(b"#warning about /*\n#if __has_include(<w.h>)\n#endif\n"), [b"w.h".to_vec()]);
        assert_eq!(probes(b"#if __has_include(<a/*b.h>)\n#endif\n"), [b"a/*b.h".to_vec()]);
        assert_eq!(probes(b"/* #if __has_include(<c.h>) */\n"), [b"c.h".to_vec()]);
        for odd in [
            &b"#if __has_\\\rinclude(<x.h>)\n"[..],
            b"// x\r#define H __has_include\n",
            b"int x;\0\n",
            b"#if __has_include(<x.h>) ??/\n#endif\n",
            b"#define CAT(a, b) a ??=??= b\n",
            b"#embed \"data.bin\"\n",
            b"  %: embed <data.bin>\n",
            b"#/* the data */ embed \"data.bin\"\n",
            b"/* the data: */ #embed \"data.bin\"\n",
            b"#if __has_embed(\"data.bin\")\n#endif\n",
            b"/* __DATE__ */\n",
            b"const char *when = \"\" __TIME__;\n",
            b"const char *when = __TIMESTAMP__;\n",
            b"#define H __has_include\n",
            b"#if APPLY(__has_include, <x.h>)\n",
            b"#if __has_include(X)\n",
            // What could write a line taken for a line marker.
            b"# 1 \"s.c\" 2\n",
            b"#/**/ 3 \"s.c\" 2\n",
            b"/* a\n */ # 1 \"s.c\" 2\n",
            b"#define E(x) x\nE(#) 1 \"s.c\" 2\n",
            b"E( /* c */ %:) 1 \"s.c\" 2\n",
            b"E(#\n) 1 \"s.c\" 2\n",
            // What only looks like a comment.
            b"#if __has_include(MACRO) /* c */\n",
            b"#if __has_include(<y.h> /* c */)\n",
            b"#if CALL(__has_include, <y.h>) /* c */\n",
            b"#if __has_include(X) // see */\n",
            b"#if __has_include(<a//b.h>) || __has_include(Y)\n",
            b"#if __has_include(Y) || __has_include(<a*/b.h>)\n",
            b"#if __has_include(<a/*b.h>) || __has_include(Y)\n",
            // Comment marks that share a character.
            b"#if 1 /**/* __has_include(HDR)\n",
            b"#if __has_include(HDR) */**/1\n",
            b"#if 1 /**// (1 + __has_include(HDR))\n",
            // A byte order mark the compilers pass over.
            b"\xEF\xBB\xBF#embed \"d.bin\"\n",
            b"\xEF\xBB\xBF# 1 \"s.c\" 2\n",
            // Blanks one compiler keeps in a name and the other does not.
            b"#if __has_include(< x.h >)\n",
        ] {
            assert!(scanned(odd).is_err(), "{}", String::from_utf8_lossy(odd));
        }
        for fine in [
            &b"#define STR(x) #x\n#define CAT(a, b) a ## b\n#line 7 \"orig.c\"\n"[..],
            b"# define V(x) __attribute__ ((__visibility__ (#x)))\n",
            b"int weak; // #weak + (#shared != 0)\n",
            b"/*\n * C99 (#include \"omp.h\" not acceptable)\n */\n",
            b"/* see issue #181 and tables.html#65 */\n",
            b"#endif // __has_include\n",
            b"/* a\n   __has_include argument (GCC PR 80005).  */\n",
            b"#ifdef __has_include\n#if defined(__has_include) && defined __has_include\n#endif\n#endif\n",
            b"int embedded;\n#define embed 1\n",
            // A name pasted from pieces is a stated limit (decision 15).
            b"#define HAS(h) __has_ ## include(h)\n#if HAS(<x.h>)\n#endif\n",
        ] {
            assert!(scanned(fine).is_ok(), "{}: {:?}", String::from_utf8_lossy(fine), scanned(fine));
        }
        // A `<name>` in a macro, or a macro's argument, is expanded.
        let expanded = |bytes: &[u8]| scanned(bytes).unwrap().iter().map(|probe| probe.expanded).collect::<Vec<_>>();
        assert_eq!(expanded(b"#if __has_include(<x.h>) || (__has_include(<y.h>))\n"), [false, false]);
        assert_eq!(expanded(b"#if ID(__has_include(<x.h>))\n#  define X __has_include(<y.h>)\n"), [true, true]);
        assert_eq!(expanded(b"#if defined(X) && __has_include(<x.h>)\n"), [false]);
        assert_eq!(expanded(b"# if __has_include (<linux/x.h>)\n#if defined __has_include && __has_include (<y.h>)\n"), [false, false]);
        assert_eq!(expanded(b"#if CALL __has_include(<y.h>))\n#if ID /* c */ (__has_include(<z.h>))\n"), [true, true]);
        assert_eq!(expanded(b"#if 1 + X && __has_include(<y.h>)\n"), [true]);
    }

    /// The directories a compile names, from its arguments; flags that
    /// make others are not followed.
    #[test]
    fn reads_the_directories_a_compile_names() {
        let args = |args: &[&str]| args.iter().map(std::ffi::OsString::from).collect::<Vec<_>>();
        let given = Given::from_args(&args(&["-Iinc/", "-I", "two", "-iquote", "q", "-isystem", "sys", "-idirafter", "late", "-include", "x.h"]), false);
        assert!(given.all.starts_with(&[b"inc/".to_vec(), b"two".to_vec(), b"q".to_vec(), b"sys".to_vec(), b"late".to_vec()]));
        assert!(given.user.starts_with(&[b"inc/".to_vec(), b"two".to_vec(), b"q".to_vec()]));
        assert!(Given::same(b"inc/", b"inc") && Given::same(b"inc", b"inc/") && Given::same(b"/", b"/") && !Given::same(b"inc", b"inc2"));
        for flag in ["-iprefix", "-iwithprefix", "-iwithprefixbefore", "-isysroot", "--sysroot=/x"] {
            assert!(Given::from_args(&args(&[flag, "d"]), false).unfollowed.is_some(), "{flag}");
        }
    }

    /// What `-dD` printed of a macro is checked exactly.
    #[test]
    fn macros_are_read_as_the_run_printed_them() {
        let body = |rest: &[u8]| {
            let mut probes = Vec::new();
            macro_body(rest, &mut probes).map(|()| probes)
        };
        assert!(body(b"M # 1 \"s.c\" 2").is_err(), "an object-like macro with a #");
        assert!(body(b"STR(x) #x").is_ok());
        assert!(body(b"H __has_include").is_err());
        assert!(body(b"WHEN __DATE__").is_err());
        assert!(body(b"HAVE(name) __has_include(<name.h>)").is_err(), "a parameter in the name");
        assert!(body(b"HAVE(...) __has_include(<__VA_ARGS__>)").is_err());
        assert!(body(b"HAVE(name) __has_include(<other.h>)").is_ok());
        let probes = body(b"_GLIBCXX_USE_TBB_PAR_BACKEND __has_include(<tbb/tbb.h>)").unwrap();
        assert_eq!(probes, [Probe { angled: true, spelling: b"tbb/tbb.h".to_vec(), expanded: true }]);
        let mut tracker = Tracker::default();
        tracker.marker(b"s.c", false, false, false);
        assert!(tracker.macro_line(b"#define linux 1\n"));
        assert!(tracker.macro_line(b"#define HAS __has_include(<linux/version.h>)\n"));
        assert!(!tracker.macro_line(b"int x;\n"));
        let followed = tracker.finish(false).unwrap();
        assert!(followed.defined.contains(b"linux".as_slice()));
        let said = b"#include \"...\" search starts here:\n#include <...> search starts here:\n /usr/include\nEnd of search list.\n";
        let lookups = Lookups::new(said, followed, Vec::new(), &[], true, &Given::default());
        assert!(lookups.is_err_and(|why| why.contains("linux/version.h")), "a macro in a name the compiler expands");
        // The compilers' dynamic macros are no `-dD` names.
        let mut tracker = Tracker::default();
        tracker.marker(b"s.c", false, false, false);
        let expanded = Probe { angled: true, spelling: b"a/__LINE__.h".to_vec(), expanded: true };
        let lookups = Lookups::new(said, tracker.finish(false).unwrap(), vec![expanded], &[], true, &Given::default());
        assert!(lookups.is_err());
        // Nor the built-in function-like ones.
        let mut tracker = Tracker::default();
        tracker.marker(b"s.c", false, false, false);
        let expanded = Probe { angled: true, spelling: b"__has_attribute(packed).h".to_vec(), expanded: true };
        assert!(Lookups::new(said, tracker.finish(false).unwrap(), vec![expanded], &[], true, &Given::default()).is_err());
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
        assert_eq!(directives.directives[1].from, b"a.h");
        assert_eq!(directives.directives[1].by, Some(0));
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
        let out = scanned(b"#if __has_include(<tbb/tbb.h>)\n#  define X __has_include( \"y.h\" )\n#endif // __has_include\n#ifdef __has_include\n#if defined(__has_include) && defined __has_include\nint my__has_include;\n/* __has_include */ /* a */\n/* two\n   lines: __has_include argument */\n// spliced \\\n__has_include\n").unwrap();
        assert_eq!(out, vec![Probe { angled: true, spelling: b"tbb/tbb.h".to_vec(), expanded: false }, Probe { angled: false, spelling: b"y.h".to_vec(), expanded: false }]);
        for odd in [&b"#if __has_include(HEADER)\n"[..], b"#define H __has_include\n", b"#if __has_include_next(X)\n", b"#if __has_include(<x.h\n", b"#define H \"//\" __has_include\n", b"/* */ __has_include\n", b"#define H \"/*\" __has_include\n", b"R\"x(/*)x\" __has_include\n"] {
            assert!(scanned(odd).is_err(), "{}", String::from_utf8_lossy(odd));
        }
    }
}
