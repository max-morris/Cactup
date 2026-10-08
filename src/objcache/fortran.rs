//! Fortran (§18.10): what a gfortran compile reads and writes, and how a
//! serving cache compiles it.
//!
//! **What it reads.** Besides the source: module files (`use`), included
//! files (`include`), and gfortran's own pre-included header. No preprocessor
//! output shows them, since Cactus has preprocessed the source already and
//! gfortran does not run one on it. gfortran says itself which files it read
//! when it is run with `-cpp -undef -M -fsyntax-only` (the dependency run,
//! [`dependencies`]): each file by the name it was found under, module files
//! included — the intrinsic ones by their full path — and the module files it
//! writes as targets. That run needs the C preprocessor, which the compile
//! does not run, so the source must read to that preprocessor as it is
//! ([`preprocessor_agrees`]). It writes module files, and reads back the
//! ones it wrote, so it runs in an empty directory of its own with the
//! compile's working directory first among its `-I` directories; since its
//! search order is still not the compile's, each file it read is checked
//! against the order the compile searches in ([`read_inputs`]).
//!
//! **What it writes.** The object, and one module file (`.mod`, `.smod` for a
//! submodule) per module the source defines, into the working directory
//! (Cactus compiles from the configuration's `scratch`; a compile that names a
//! module directory of its own, `-J`, is not cached). An entry keeps them
//! all; a hit puts back each module file whose bytes differ from what is
//! there, as gfortran itself leaves alone a module file that would not change.
//!
//! **Paths** (decision 13). gfortran writes a source's name into the object
//! for its runtime messages, and no prefix map reaches it: the name it was
//! given ("In file '...', around line 7"), and the name in a line marker, or
//! else the one it was given ("At line 7 of file ..."). So under the path map
//! a compile names its source by the path from its working directory
//! (`../build/Thorn/x.f90`, the same in every tree), and a source with line
//! markers is compiled as a copy whose markers name their files by mapped
//! names ([`Renamed`]): under the source's own file name (a module file
//! records it), in `.cactup/` beside the source, with the source's directory
//! first among the `-I` directories, where Fortran `include` would have
//! looked first. Messages that name the source so, or a mapped name, are
//! given back their real names ([`Renamed::rewrite`]).

use super::compile::Compile;
use super::hash::Hasher;
use super::identity::{Compiler, Family};
use super::key::{self, naming, Named, PathMap};
use crate::Res;
use anyhow::{bail, Context};
use std::ffi::{OsStr, OsString};
use std::io::Read as _;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;

/// Does `text` have only line markers where a line begins with `#`? Any
/// other such line is a directive to the preprocessor of the dependency
/// run, which the compile does not run; and it would leave no trace in that
/// preprocessor's output for [`preprocessor_agrees`] to see.
pub fn check_text(text: &[u8]) -> Result<(), String> {
    for line in text.split(|b| *b == b'\n') {
        if line.first() == Some(&b'#') && !is_marker(line)? {
            return Err(format!("the source has a preprocessor line ({}), which the cache does not follow", String::from_utf8_lossy(line).trim_end()));
        }
    }
    Ok(())
}

/// Is `line` a line marker as Cactus writes them into a Fortran build copy
/// (`# 12 "file"`, or `# 12` alone)? `Err`: it begins as one and its name
/// cannot be read.
fn is_marker(line: &[u8]) -> Result<bool, String> {
    if naming(line, b"# ")?.is_some() {
        return Ok(true);
    }
    let digits = line.strip_prefix(b"# ").unwrap_or_default();
    Ok(!digits.is_empty() && digits.iter().all(u8::is_ascii_digit))
}

/// A file name as a line marker writes it: a C string.
fn quoted(name: &[u8]) -> Vec<u8> {
    let mut out = vec![b'"'];
    for byte in name {
        if matches!(byte, b'"' | b'\\') {
            out.push(b'\\');
        }
        out.push(*byte);
    }
    out.push(b'"');
    out
}

/// Does `text` have a line marker that names a file?
fn names_files(text: &[u8]) -> Result<bool, String> {
    for line in text.split(|b| *b == b'\n') {
        if naming(line, b"# ")?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `text` with every line marker's file named by its mapped name. Line
/// numbers stay what they were: a marker says what the next line is.
pub fn mapped_text(text: &[u8], map: &PathMap) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(text.len() + 128);
    let mut lines = text.split(|b| *b == b'\n').peekable();
    while let Some(line) = lines.next() {
        match naming(line, b"# ")? {
            Some(marker) => {
                out.extend_from_slice(&marker.head[..marker.head.len() - 1]);
                out.extend(quoted(&map.apply(&marker.name)));
                out.extend_from_slice(&marker.tail[1..]);
            }
            None => out.extend_from_slice(line),
        }
        if lines.peek().is_some() {
            out.push(b'\n');
        }
    }
    Ok(out)
}

/// `to` as a path from the directory `from`, both absolute and physical.
fn relative(from: &Path, to: &Path) -> PathBuf {
    let (from, to): (Vec<_>, Vec<_>) = (from.components().collect(), to.components().collect());
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut out = PathBuf::new();
    for _ in common..from.len() {
        out.push("..");
    }
    for part in &to[common..] {
        out.push(part);
    }
    out
}

/// How a compile under the path map names its source (decision 13).
#[derive(Debug)]
pub struct Renamed {
    /// The name the compile is given: the file compiled, by its path from
    /// the working directory.
    given: PathBuf,
    /// The copy, when the source's line markers name files: where it goes
    /// (`.cactup/` beside the source, under the source's own name) and what
    /// it holds (the source with those names mapped).
    copy: Option<(PathBuf, Vec<u8>)>,
    /// The source's own directory, by its physical path.
    source_dir: PathBuf,
    /// The source as the recipe named it.
    original: OsString,
}

impl Renamed {
    /// How the compile of `source` (as the recipe named it, read as `text`)
    /// in `cwd` (a physical path) names it under `map`: by its path from
    /// `cwd`, and if `text` has line markers that name files, as a copy with
    /// those mapped ([`Renamed::write_copy`]).
    pub fn new(cwd: &Path, source: &Path, text: &[u8], map: &PathMap) -> Res<Self> {
        let joined = cwd.join(source);
        let name = joined.file_name().with_context(|| format!("{} names no file", source.display()))?;
        let dir = joined.parent().unwrap_or(Path::new("/"));
        let source_dir = std::fs::canonicalize(dir).with_context(|| format!("Failed to resolve {}", dir.display()))?;
        let copy = match names_files(text).map_err(anyhow::Error::msg)? {
            false => None,
            true => Some((source_dir.join(".cactup").join(name), mapped_text(text, map).map_err(anyhow::Error::msg)?)),
        };
        let compiled = copy.as_ref().map_or_else(|| source_dir.join(name), |(path, _)| path.clone());
        Ok(Self { given: relative(cwd, &compiled), copy, source_dir, original: source.as_os_str().to_owned() })
    }

    /// Write the copy, if there is one: whole (a temporary file renamed into
    /// place), and left there like the build copy itself; another compile of
    /// the same source writes the same bytes. Only a compile for the store
    /// reads it, so only a serving build writes it.
    pub fn write_copy(&self) -> Res<()> {
        let Some((copy, text)) = &self.copy else { return Ok(()) };
        let dir = copy.parent().expect("a copy is inside its directory");
        std::fs::create_dir_all(dir).with_context(|| format!("Failed to create {}", dir.display()))?;
        let mut temp = tempfile::Builder::new()
            .prefix(".copy-")
            .tempfile_in(dir)
            .with_context(|| format!("Failed to create a file in {}", dir.display()))?;
        std::io::Write::write_all(&mut temp, text).context("Failed to write a copy of the source")?;
        temp.persist(copy).with_context(|| format!("Failed to write {}", copy.display()))?;
        Ok(())
    }

    /// The directory copies are made in.
    fn copies(&self) -> PathBuf {
        self.source_dir.join(".cactup")
    }

    /// The include flag that has gfortran look in the source's directory
    /// right after the copy's, as it would have for the source itself (only
    /// a copy needs it).
    fn include_flag(&self) -> Option<OsString> {
        self.copy.as_ref().map(|_| {
            let mut flag = OsString::from("-I");
            flag.push(&self.source_dir);
            flag
        })
    }

    /// What the compile's messages say in place of real names, and the real
    /// names: the name the compile was given, the configuration directory's
    /// mapped name, the Cactus root's mapped name (§18.10).
    pub fn rewrite(&self, conf: &super::BuildConf) -> key::Rewrite {
        let dir = |path: &Path| [path.as_os_str().as_bytes(), b"/"].concat();
        key::Rewrite::new(vec![
            (self.given.as_os_str().as_bytes().to_vec(), self.original.as_bytes().to_vec()),
            (key::CONFIG_NAME.as_bytes().to_vec(), dir(&conf.config_dir)),
            (key::ROOT_NAME.as_bytes().to_vec(), dir(&conf.cactus_root)),
        ])
    }
}

/// What a Fortran compile reads and writes, as its dependency run says.
#[derive(Debug, Clone, PartialEq)]
pub struct Dependencies {
    /// Every file read besides the source, by its absolute path.
    pub inputs: Vec<PathBuf>,
    /// The module files written, by file name.
    pub modules: Vec<String>,
}

/// The state of a Fortran compile the cache keyed: kept for the compile and
/// for the check after it.
#[derive(Debug)]
pub struct Fortran {
    /// How the compile names its source, under the path map.
    pub renamed: Option<Renamed>,
    /// Whether the copy was written (by a serving build, for its compile).
    copy_written: bool,
    /// The module files the compile writes.
    pub modules: Vec<String>,
    /// The dependency runs' working directory, where they write their
    /// module files.
    private: tempfile::TempDir,
    /// The arguments of the dependency run.
    args: Vec<OsString>,
    /// The source, by its absolute physical path: what the dependency run
    /// reads.
    source: PathBuf,
    /// Where the compile looks for a module file, in its order: its working
    /// directory, the directory of the file it compiles, its `-I`
    /// directories. All physical paths.
    module_dirs: Vec<PathBuf>,
    /// The compile's working directory, by its physical path.
    cwd: PathBuf,
}

impl Fortran {
    /// The arguments a compile for the store runs with in place of `args`,
    /// the source at `source_at` among them: the source by the name the map
    /// gives it, and for a copy, the source's directory first among the `-I`
    /// directories.
    pub fn arguments(&self, args: &[OsString], source_at: usize) -> Vec<OsString> {
        let mut out: Vec<OsString> = args.to_vec();
        if let Some(renamed) = &self.renamed {
            out[source_at] = renamed.given.as_os_str().to_owned();
            if let Some(flag) = renamed.include_flag() {
                out.insert(0, flag);
            }
        }
        out
    }
}

/// The Fortran parts of a key: the text the compile reads (with the name it
/// is given and the module files it writes), and the files it reads
/// besides; with what they looked like (`seen`, for the check after the
/// compile).
pub struct Keyed {
    pub fortran: Fortran,
    pub text: String,
    pub text_bytes: u64,
    pub files: String,
    pub count: usize,
    pub seen: String,
}

/// `args` without `-D` and `-U` (each with its value): the compile does not
/// preprocess, so they do nothing to it, and the runs that do preprocess
/// must not have them either.
fn without_macros(args: Vec<OsString>) -> Vec<OsString> {
    let mut out = Vec::with_capacity(args.len());
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_bytes() {
            b"-D" | b"-U" => {
                args.next();
            }
            flag if flag.starts_with(b"-D") || flag.starts_with(b"-U") => {}
            _ => out.push(arg),
        }
    }
    out
}

/// Key what a Fortran `compile` reads (§18.10), given by the arguments
/// `args` (the command line without the program) and run in `cwd`. `in_dir`
/// is a directory of the build's for the dependency runs. `serving`: the
/// compile is to be stored, and reads the copy, which is then written.
pub fn key(compiler: &Compiler, name: &OsStr, compile: &Compile, args: &[OsString], cwd: &Path, map: Option<&PathMap>, in_dir: &Path, serving: bool) -> Res<Keyed> {
    if compiler.family != Family::Gfortran {
        bail!("Fortran is cached for gfortran only");
    }
    // The dependency run runs elsewhere: a directory it is given by a
    // relative name would be another directory there.
    let mut include_dirs = Vec::new();
    let mut flags = compile.preprocess.iter();
    while let Some(flag) = flags.next() {
        if flag == "-I" {
            let dir = Path::new(flags.next().map(OsString::as_os_str).unwrap_or_default());
            if dir.is_relative() {
                bail!("an include directory is named by a relative path");
            }
            include_dirs.push(std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_owned()));
        }
    }
    let cwd = std::fs::canonicalize(cwd).with_context(|| format!("Failed to resolve {}", cwd.display()))?;
    // The source, read once: what the compile reads is this, or a copy of it.
    let joined = cwd.join(&compile.source);
    let source_dir = joined.parent().and_then(|dir| std::fs::canonicalize(dir).ok()).with_context(|| format!("Failed to resolve {}", joined.display()))?;
    let source = source_dir.join(joined.file_name().with_context(|| format!("{} names no file", joined.display()))?);
    let mut file = std::fs::File::open(&source).with_context(|| format!("Failed to open {}", source.display()))?;
    let mut text = Vec::new();
    file.read_to_end(&mut text).with_context(|| format!("Failed to read {}", source.display()))?;
    check_text(&text).map_err(anyhow::Error::msg)?;
    let renamed = map.map(|map| Renamed::new(&cwd, &compile.source, &text, map)).transpose()?;
    let copy_written = match &renamed {
        Some(renamed) if serving && renamed.copy.is_some() => {
            renamed.write_copy()?;
            true
        }
        _ => false,
    };
    let (compiled, given) = match &renamed {
        Some(Renamed { copy: Some((_, copy)), given, .. }) => (copy.clone(), given.clone()),
        Some(renamed) => (text.clone(), renamed.given.clone()),
        None => (text.clone(), compile.source.clone()),
    };
    // Where the compile looks for module files: its working directory, the
    // directory of the file it reads, then its `-I` directories (for a copy,
    // the source's directory first among those).
    let mut module_dirs = vec![cwd.clone()];
    match &renamed {
        Some(renamed) if renamed.copy.is_some() => module_dirs.extend([renamed.copies(), source_dir.clone()]),
        _ => module_dirs.push(source_dir.clone()),
    }
    module_dirs.extend(include_dirs);
    let private = tempfile::Builder::new()
        .prefix(".fortran-")
        .tempdir_in(in_dir)
        .with_context(|| format!("Failed to create a directory in {}", in_dir.display()))?;
    // The runs that preprocess: in their own directory, finding module
    // files in the compile's working directory after their own; the source
    // itself, by its absolute path; the compile's arguments otherwise, but
    // for what only a preprocessor reads.
    let (without_output, source_at) = compile.without_output(args);
    let mut base = without_output;
    base[source_at] = source.as_os_str().to_owned();
    let mut first = OsString::from("-I");
    first.push(&cwd);
    base.insert(0, first);
    let base = without_macros(base);
    let agrees = preprocessor_agrees(compiler, name, &base, private.path(), &text).map_err(anyhow::Error::msg);
    agrees?;
    let mut run_args = base;
    run_args.extend(["-cpp", "-undef", "-M", "-fsyntax-only", "-v"].map(OsString::from));
    let mut fortran = Fortran { renamed, copy_written, modules: Vec::new(), private, args: run_args, source, module_dirs, cwd };

    let deps = dependencies(compiler, name, &fortran)?;
    fortran.modules = deps.modules.clone();
    let mut hasher = Hasher::new("fortran text");
    hasher.feed(given.as_os_str().as_bytes());
    hasher.feed(&compiled);
    for module in &deps.modules {
        hasher.feed(module.as_bytes());
    }
    let text_bytes = compiled.len() as u64;
    let (files, seen, count) = read_inputs(&deps, map, &fortran)?;
    Ok(Keyed { text: hasher.hex(), text_bytes, files, count, seen, fortran })
}

/// Does the preprocessor of the dependency run, run with `args` (the source
/// among them) in `dir`, read the source as `text`, its lines as they are?
/// It runs in traditional mode with every macro it can drop dropped
/// (`-undef`), and still acts on more than a Fortran source should give it:
/// a `/*` (which swallows lines up to the next `*/`), a line ending in `\`
/// (blanks after it too), a lone carriage return, the names it still
/// defines (`__FILE__`, `_OPENMP`, `_REENTRANT` under `-fopenmp`), trigraphs
/// under `-trigraphs`. Rather than list them, its output is compared with
/// the source: every line but its line markers must be the source's, and in
/// order (lines blank on both sides aside, as it may stand a marker for a
/// run of them). Otherwise the dependency run would not have read what the
/// compile reads, and the compile is not cached.
fn preprocessor_agrees(compiler: &Compiler, name: &OsStr, args: &[OsString], dir: &Path, text: &[u8]) -> Result<(), String> {
    let mut command = Command::new(&compiler.path);
    command.arg0(name).args(args).args(["-cpp", "-undef", "-E"]).current_dir(dir);
    key::in_english(&mut command);
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("the preprocessor could not be run ({e})"))?;
    key::PREPROCESSOR.store(child.id() as i32, Ordering::SeqCst);
    let out = child.wait_with_output();
    key::PREPROCESSOR.store(0, Ordering::SeqCst);
    let out = out.map_err(|e| format!("the preprocessor could not be waited for ({e})"))?;
    if !out.status.success() {
        return Err(format!("the source does not pass the dependency run's preprocessor ({})", out.status));
    }
    let lines = |bytes: &[u8]| -> Vec<Vec<u8>> {
        bytes
            .split(|b| *b == b'\n')
            .filter(|line| !line.starts_with(b"#") && !line.iter().all(|b| matches!(b, b' ' | b'\t')))
            .map(<[u8]>::to_vec)
            .collect()
    };
    match lines(&out.stdout) == lines(text) {
        true => Ok(()),
        false => Err("the source reads otherwise to the preprocessor of the dependency run".to_owned()),
    }
}

/// Do the files `fortran`'s dependency run lists still say what they said
/// (`files`, `seen`), and does it still list the same?
pub fn still_holds(compiler: &Compiler, name: &OsStr, map: Option<&PathMap>, fortran: &Fortran, files: &str, seen: &str) -> bool {
    let Ok(deps) = dependencies(compiler, name, fortran) else { return false };
    deps.modules == fortran.modules
        && read_inputs(&deps, map, fortran).is_ok_and(|(now_files, now_seen, _)| now_files == files && now_seen == seen)
}

/// The files the dependency run listed, digested as `key::read_files` does,
/// with the source itself (and the copy, when the compile reads one: below);
/// their bytes are checked again after the compile, like any file read.
///
/// The dependency run searches in another order than the compile: its own
/// directory, then the source's, then the compile's working directory (an
/// `-I` directory for it) for both module files and included files. So
/// every module file it read must be the first of its name in the compile's
/// order too (`module_dirs`); no included file may be found under the
/// working directory, which the compile does not search for them; and for a
/// copy, none may be one that the copy's directory, searched first by the
/// compile, has too.
fn read_inputs(deps: &Dependencies, map: Option<&PathMap>, fortran: &Fortran) -> Res<(String, String, usize)> {
    let mut named = Named::new();
    let mapped = |path: &Path| map.map_or_else(|| path.as_os_str().as_bytes().to_vec(), |map| map.apply(path.as_os_str().as_bytes()));
    let copies = fortran.renamed.as_ref().filter(|renamed| renamed.copy.is_some()).map(Renamed::copies);
    for input in &deps.inputs {
        let physical = std::fs::canonicalize(input).unwrap_or_else(|_| input.clone());
        let file_name = input.file_name().unwrap_or_default();
        // A module file by its name and by what it is (gfortran writes them
        // gzip-compressed): a text file named so is an included file.
        // A file named like a module file may be one, whatever its bytes
        // (gfortran reads module files through zlib, which takes an
        // uncompressed file as it is): it is checked as a module file. Only
        // one that is gzip-compressed, as gfortran writes them, is surely
        // not an included file; any other is checked as one too.
        let named_module = input.extension().is_some_and(|ext| ext == "mod" || ext == "smod");
        if named_module {
            let first = fortran.module_dirs.iter().find(|dir| dir.join(file_name).exists());
            if first.is_some_and(|dir| Some(dir.as_path()) != physical.parent()) {
                bail!("the compile would find another module file of the name {} first", file_name.to_string_lossy());
            }
        }
        if !(named_module && is_gzip(input)) {
            if input.starts_with(&fortran.cwd) || physical.starts_with(&fortran.cwd) {
                bail!("an included file would be found under the working directory, where the compile does not look for one");
            }
            if copies.is_some() {
                // The compile of a copy looks first in the copy's directory,
                // which has copies of other sources and copies whose build
                // copy is gone; and a name with `..` in it (the dependency
                // run prints each as it was written, after the directory it
                // was found in) leads elsewhere from there than from the
                // source's directory.
                if copies.as_ref().is_some_and(|copies| copies.join(file_name).exists()) {
                    bail!("an included file of that name is beside the copy, where the compile would look first");
                }
                if input.components().any(|part| part == std::path::Component::ParentDir) {
                    bail!("an included file is named with \"..\", which leads elsewhere from the copy");
                }
            }
        }
        named.insert((mapped(input), input.clone()), true);
    }
    let copy = fortran.renamed.as_ref().and_then(|renamed| renamed.copy.as_ref());
    // The source, keyed by its bytes where it is what the compile reads.
    if copy.is_none() {
        named.insert((b"source".to_vec(), fortran.source.clone()), true);
    }
    let (files, mut seen) = key::read_files(&named, map)?;
    // Where a copy is compiled, its bytes (the source's, mapped) are the
    // key's text already; the source, whose line markers name this tree,
    // and the copy (written only by a build that serves, so that one that
    // records keys alike) are only looked at, for the check after the
    // compile: the dependency runs read the source.
    if let Some((copy_path, _)) = copy {
        let mut looked_at = Named::new();
        looked_at.insert((b"source".to_vec(), fortran.source.clone()), true);
        if fortran.copy_written {
            looked_at.insert((mapped(copy_path), copy_path.clone()), true);
        }
        let (bytes, also_seen) = key::read_files(&looked_at, map)?;
        let mut hasher = Hasher::new("seen with the source and its copy");
        for part in [&seen, &bytes, &also_seen] {
            hasher.feed(part.as_bytes());
        }
        seen = hasher.hex();
    }
    Ok((files, seen, named.len()))
}

/// Does the file at `path` begin as a gzip stream does?
fn is_gzip(path: &Path) -> bool {
    let mut magic = [0u8; 2];
    std::fs::File::open(path).and_then(|mut file| file.read_exact(&mut magic)).is_ok() && magic == [0x1f, 0x8b]
}

/// Run the dependency run of `fortran`, and read what it says.
fn dependencies(compiler: &Compiler, name: &OsStr, fortran: &Fortran) -> Res<Dependencies> {
    // What an earlier run wrote there is no input of this one.
    for entry in std::fs::read_dir(fortran.private.path()).with_context(|| format!("Failed to read {}", fortran.private.path().display()))? {
        let entry = entry.context("Failed to read a module directory")?;
        std::fs::remove_file(entry.path()).with_context(|| format!("Failed to remove {}", entry.path().display()))?;
    }
    let mut command = Command::new(&compiler.path);
    command.arg0(name).args(&fortran.args).current_dir(fortran.private.path());
    key::in_english(&mut command);
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("Failed to run {} for the dependencies", compiler.path.display()))?;
    key::PREPROCESSOR.store(child.id() as i32, Ordering::SeqCst);
    let out = child.wait_with_output();
    key::PREPROCESSOR.store(0, Ordering::SeqCst);
    let out = out.context("Failed to wait for the dependency run")?;
    if !out.status.success() {
        bail!("the dependency run failed ({})", out.status);
    }
    key::flags_from_elsewhere(compiler.family, compiler.specs.as_deref(), &out.stderr).map_err(anyhow::Error::msg)?;
    let copies = fortran.renamed.as_ref().filter(|renamed| renamed.copy.is_some()).map(Renamed::copies);
    parse_rule(&out.stdout, &fortran.source, copies.as_deref()).map_err(anyhow::Error::msg)
}

/// Read the rule a dependency run printed: `<targets>: <the source>
/// <inputs>`, lines joined by `\`. The run's working directory is its own:
/// the module files it wrote are the targets named there (no directory),
/// and an input named there is a module the source itself defines and
/// uses. Every other input has an absolute name. The source, `compiled`, is
/// the first input; nothing else may be read from `copies`, the directory
/// copies are made in. A name the rule had to escape (a space, `#`, `$` in
/// it) is not read, and the compile is not cached.
fn parse_rule(out: &[u8], compiled: &Path, copies: Option<&Path>) -> Result<Dependencies, String> {
    let text = std::str::from_utf8(out).map_err(|_| "the dependency run named a file that is not UTF-8".to_owned())?;
    let joined = text.replace("\\\n", " ");
    let words: Vec<&str> = joined.split_whitespace().collect();
    let colon = words.iter().position(|word| word.ends_with(':')).ok_or("the dependency run printed no rule")?;
    if words.iter().skip(colon + 1).any(|word| word.ends_with(':')) {
        return Err("the dependency run printed more than one rule".to_owned());
    }
    if let Some(word) = words.iter().find(|word| word.contains(['\\', '$', '#']) || word[..word.len() - usize::from(word.ends_with(':'))].contains(':')) {
        return Err(format!("the dependency run named a file the cache does not read the name of ({word})"));
    }
    let own = |name: &str| !name.contains('/');
    let mut modules = Vec::new();
    for target in words[..=colon].iter().map(|word| word.trim_end_matches(':')) {
        let is_module = target.ends_with(".mod") || target.ends_with(".smod");
        match (own(target), is_module) {
            (true, true) => modules.push(target.to_owned()),
            (true, false) => {} // the object
            _ => return Err(format!("the dependency run names an output the cache does not follow ({target})")),
        }
    }
    let mut prerequisites = words[colon + 1..].iter();
    let source = prerequisites.next().ok_or("the dependency run named no source")?;
    if Path::new(source) != compiled {
        return Err(format!("the dependency run named another source ({source})"));
    }
    let mut inputs = Vec::new();
    for input in prerequisites {
        if own(input) {
            // A module the source defines and then uses: its own output.
            if !modules.iter().any(|module| module == input) {
                return Err(format!("the dependency run read a module file of its own that it did not write ({input})"));
            }
            continue;
        }
        if !Path::new(input).is_absolute() {
            return Err(format!("the dependency run named a file by a relative path ({input})"));
        }
        if copies.is_some_and(|dir| Path::new(input).starts_with(dir)) {
            return Err(format!("the compile would read a file beside the copy ({input})"));
        }
        inputs.push(PathBuf::from(input));
    }
    inputs.sort();
    inputs.dedup();
    Ok(Dependencies { inputs, modules })
}

/// The sources of [`relocates`]' trial: a Cactus Fortran compile in
/// miniature. A module whose procedures write and check an array's bounds
/// (so that the object holds the source's names for runtime messages, both
/// kinds), that includes a file found beside the source; once with a line
/// marker naming the file it was made from, once without one.
const TRIAL_MODULE: &str = "module cactup_trial_m\n  include 'cactup_trial.inc'\ncontains\n  subroutine cactup_trial_say(u, a, n)\n    integer, intent(in) :: u, n\n    real, intent(inout) :: a(n)\n    real, allocatable :: b(:)\n    a(n) = 1\n    allocate (b(n))\n    allocate (b(n))\n    write (u, *) 'trial', cactup_trial_seven, a(n)\n  end subroutine\nend module cactup_trial_m\n";
const TRIAL_INCLUDE: &str = "integer, parameter :: cactup_trial_seven = 7\n";

/// Compile the trial in a tree at `root` with a configuration called
/// `config`, the way a serving cache compiles Fortran. The object and the
/// module file of each source, if every compile succeeds.
fn trial(compiler: &Path, name: &OsStr, root: &Path, config: &str) -> Option<Vec<Vec<u8>>> {
    let config = root.join("configs").join(config);
    let (src, build, scratch) = (root.join("src"), config.join("build/T"), config.join("scratch"));
    for dir in [&src, &build, &scratch] {
        std::fs::create_dir_all(dir).ok()?;
    }
    std::fs::write(build.join("cactup_trial.inc"), TRIAL_INCLUDE).ok()?;
    let map = PathMap::for_trial(root, &config);
    let mut out = Vec::new();
    for (file, marker) in [("plain.f90", None), ("marked.f90", Some(src.join("marked.F90")))] {
        let text = match &marker {
            Some(original) => format!("# 1 \"{}\"\n{TRIAL_MODULE}", original.display()),
            None => TRIAL_MODULE.to_owned(),
        };
        let source = build.join(file);
        std::fs::write(&source, &text).ok()?;
        let renamed = Renamed::new(&scratch, &source, text.as_bytes(), &map).ok()?;
        renamed.write_copy().ok()?;
        let object = build.join(format!("{file}.o"));
        let mut command = Command::new(compiler);
        command.arg0(name).args(renamed.include_flag()).args(["-g", "-fcheck=all", "-c", "-o"]).arg(&object).arg(&renamed.given);
        let status = command
            .args(map.flags())
            .current_dir(&scratch)
            .env("PWD", &scratch)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()?;
        if !status.success() {
            return None;
        }
        out.push(std::fs::read(&object).ok()?);
        out.push(std::fs::read(scratch.join("cactup_trial_m.mod")).ok()?);
    }
    Some(out)
}

/// Does gfortran, run as `name`, make one object and one module file of the
/// same sources in two places, compiled as [`Renamed`] says (decision 13)?
/// Tried in `dir`, on two trees at paths of different length with
/// configurations of different names, with debug information and runtime
/// checks on.
pub fn relocates(compiler: &Path, name: &OsStr, dir: &Path) -> bool {
    let Ok(trial_dir) = tempfile::tempdir_in(dir) else { return false };
    let Ok(at) = std::fs::canonicalize(trial_dir.path()) else { return false };
    let one = trial(compiler, name, &at.join("one/Cactus"), "a");
    let other = trial(compiler, name, &at.join("another/deeper/Cactus"), "bb");
    one.is_some() && one == other
}

/// The source of [`locale_neutral`]'s trial: bytes outside ASCII in a
/// comment and in a string.
const LOCALE_TRIAL: &[u8] = b"! d\xc3\xa9j\xc3\xa0 vu, stra\xc3\x9fe\nsubroutine cactup_trial_l(u)\n  integer, intent(in) :: u\n  write (u, *) 'caf\xc3\xa9 \xc3\xbc \xe2\x82\xac'\nend subroutine\n";

/// Does gfortran, run as `name`, make one object of [`LOCALE_TRIAL`] in this
/// process's locale and in the C locale (§18.8)?
pub fn locale_neutral(compiler: &Path, name: &OsStr, dir: &Path) -> bool {
    let Ok(trial) = tempfile::tempdir_in(dir) else { return false };
    let (source, object) = (trial.path().join("locale.f90"), trial.path().join("locale.o"));
    if std::fs::write(&source, LOCALE_TRIAL).is_err() {
        return false;
    }
    let compile = |c_locale: bool| {
        let mut command = Command::new(compiler);
        command.arg0(name).args(["-g", "-c", "-o"]).arg(&object).arg(&source).current_dir(trial.path());
        if c_locale {
            for (variable, _) in std::env::vars_os() {
                if variable.as_bytes().starts_with(b"LC_") || variable == "LANG" || variable == "LANGUAGE" {
                    command.env_remove(variable);
                }
            }
            command.env("LC_ALL", "C");
        }
        let status = command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().ok()?;
        status.success().then(|| std::fs::read(&object).ok()).flatten()
    };
    let session = compile(false);
    session.is_some() && session == compile(true)
}

/// A module file's name as an entry keeps it: a plain file name, as gfortran
/// writes them (the module's name in lowercase, `.mod` or `.smod`).
pub fn plain_module_name(name: &str) -> bool {
    let stem = name.strip_suffix(".mod").or_else(|| name.strip_suffix(".smod"));
    stem.is_some_and(|stem| !stem.is_empty() && stem.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'@')))
}

/// The module files `modules` as they are in `dir`: what an entry stores.
pub fn module_files(dir: &Path, modules: &[String]) -> Vec<(String, PathBuf)> {
    modules.iter().map(|module| (module.clone(), dir.join(module))).collect()
}

/// Do the files at `one` and `other` hold the same bytes? (A module file a
/// hit would put back is left alone when they do, as gfortran leaves it.)
pub fn same_bytes(one: &Path, other: &Path) -> bool {
    let (Ok(a), Ok(b)) = (std::fs::metadata(one), std::fs::metadata(other)) else { return false };
    a.is_file() && b.is_file() && a.len() == b.len() && std::fs::read(one).ok() == std::fs::read(other).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_line_markers_begin_with_a_hash() {
        assert_eq!(check_text(b"# 1 \"/w/Cactus/arrangements/A/T/src/x.F90\"\nmodule m\n# 7\nend module m\n"), Ok(()));
        for text in ["#define X 1\nx = X\n", "#include \"a.h\"\n", "#line 1 \"a.f90\"\n", "#if 0\n#endif\n"] {
            assert!(check_text(text.as_bytes()).unwrap_err().contains("preprocessor line"), "{text:?}");
        }
        assert!(check_text(b"# 1 \"open\n").is_err(), "a marker whose name cannot be read");
    }

    fn map() -> PathMap {
        PathMap::for_trial(Path::new("/w/Cactus"), Path::new("/w/Cactus/configs/sim"))
    }

    #[test]
    fn line_markers_name_their_files_by_mapped_names() {
        let marked = b"# 1 \"/w/Cactus/arrangements/A/T/src/x.F90\"\nmodule m\n# 12\n# 3 \"/usr/include/x.h\"\nend module m";
        let text = mapped_text(marked, &map()).unwrap();
        assert_eq!(
            String::from_utf8(text).unwrap(),
            "# 1 \"/cactup-root/arrangements/A/T/src/x.F90\"\nmodule m\n# 12\n# 3 \"/usr/include/x.h\"\nend module m"
        );
        // A name that needs escaping is written escaped; text without
        // markers is left as it is.
        let odd = mapped_text(b"# 1 \"/w/Cactus/configs/sim/build/T/a\\\"b\\\\c.f90\"\nx = 1\n", &map()).unwrap();
        assert!(odd.starts_with(b"# 1 \"/cactup-root/configs/@config/build/T/a\\\"b\\\\c.f90\"\n"), "{}", String::from_utf8_lossy(&odd));
        assert_eq!(mapped_text(b"module m\nend module m\n", &map()).unwrap(), b"module m\nend module m\n");
        assert!(!names_files(b"module m\n# 7\nend module m\n").unwrap());
        assert!(names_files(b"# 1 \"a.F90\"\nmodule m\n").unwrap());
    }

    #[test]
    fn a_source_is_named_from_the_working_directory() {
        let rel = |from: &str, to: &str| relative(Path::new(from), Path::new(to)).display().to_string();
        assert_eq!(rel("/w/Cactus/configs/sim/scratch", "/w/Cactus/configs/sim/build/T/x.f90"), "../build/T/x.f90");
        assert_eq!(rel("/v/other/Cactus/configs/renamed/scratch", "/v/other/Cactus/configs/renamed/build/T/x.f90"), "../build/T/x.f90");
        assert_eq!(rel("/w/c/scratch", "/w/c/scratch/x.f90"), "x.f90");
        assert_eq!(rel("/w/c/scratch", "/w/Cactus/arrangements/A/T/src/x.f90"), "../../Cactus/arrangements/A/T/src/x.f90");
    }

    /// Under the map: a source without markers is named from the working
    /// directory and not copied; one with markers is copied beside itself,
    /// markers mapped, and the copy is named so; both the same in any tree.
    #[test]
    fn the_map_names_a_source_and_copies_one_with_markers() {
        let tmp = tempfile::tempdir().unwrap();
        let named = |root: &Path, text: &str| {
            let config = root.join("configs/c");
            let (build, scratch) = (config.join("build/T"), config.join("scratch"));
            std::fs::create_dir_all(&build).unwrap();
            std::fs::create_dir_all(&scratch).unwrap();
            let source = build.join("x.f90");
            std::fs::write(&source, text).unwrap();
            let map = PathMap::for_trial(root, &config);
            let scratch = std::fs::canonicalize(&scratch).unwrap();
            let renamed = Renamed::new(&scratch, &source, text.as_bytes(), &map).unwrap();
            renamed.write_copy().unwrap();
            let copy = renamed.copy.as_ref().map(|(path, _)| std::fs::read_to_string(path).unwrap());
            (renamed.given.display().to_string(), renamed.include_flag().is_some(), copy)
        };
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let plain = named(&root.join("one/Cactus"), "module m\nend module m\n");
        assert_eq!(plain, ("../build/T/x.f90".to_owned(), false, None));
        let marker = |root: &Path| format!("# 1 \"{}/arrangements/A/T/src/x.F90\"\nmodule m\nend module m\n", root.display());
        let one = root.join("one/Cactus");
        let marked = named(&one, &marker(&one));
        assert_eq!((marked.0.as_str(), marked.1), ("../build/T/.cactup/x.f90", true));
        assert_eq!(marked.2.as_deref(), Some("# 1 \"/cactup-root/arrangements/A/T/src/x.F90\"\nmodule m\nend module m\n"));
        let other = root.join("another/deeper/Cactus");
        assert_eq!(named(&other, &marker(&other)), marked, "the same in another tree");
    }

    #[test]
    fn reads_the_rule_of_a_dependency_run() {
        let rule = b"user.mod other.mod use2.o: /c/x.f90 \\\n /usr/include/finclude/math-vector-fortran.h \\\n /c/scratch/m2.mod user.mod /gcc/finclude/omp_lib.mod /c/inc/vals.inc\n";
        let deps = parse_rule(rule, Path::new("/c/x.f90"), None).unwrap();
        assert_eq!(deps.modules, ["user.mod", "other.mod"]);
        let inputs: Vec<&str> = deps.inputs.iter().map(|path| path.to_str().unwrap()).collect();
        assert_eq!(inputs, ["/c/inc/vals.inc", "/c/scratch/m2.mod", "/gcc/finclude/omp_lib.mod", "/usr/include/finclude/math-vector-fortran.h"]);
        for (rule, why) in [
            (&b"x.o /c/x.f90\n"[..], "no rule"),
            (b"x.o: /c/x.f90\ny.o: /c/y.f90\n", "more than one rule"),
            (b"x.o: /c/x.f90 /a\\ b.mod\n", "does not read the name"),
            (b"x.o: /c/x.f90 /a$$b.mod\n", "does not read the name"),
            (b"x.o: /c/x.f90 /c:/x.mod\n", "does not read the name"),
            (b"x.o: /c/other.f90\n", "another source"),
            (b"x.o:\n", "no source"),
            (b"/elsewhere/m.mod x.o: /c/x.f90\n", "output the cache does not follow"),
            (b"x.o: /c/x.f90 stray.mod\n", "did not write"),
            (b"x.o: /c/x.f90 inc/vals.inc\n", "relative path"),
        ] {
            let err = parse_rule(rule, Path::new("/c/x.f90"), None).unwrap_err();
            assert!(err.contains(why), "{}: {err}", String::from_utf8_lossy(rule));
        }
        // Nothing but the copy may be read from where copies are made.
        let err = parse_rule(b"x.o: /t/.cactup/x.f90 /t/.cactup/inc.h\n", Path::new("/t/.cactup/x.f90"), Some(Path::new("/t/.cactup"))).unwrap_err();
        assert!(err.contains("beside the copy"), "{err}");
        assert!(parse_rule(b"x.o: /c/x.f90\n", Path::new("/c/x.f90"), None).unwrap().inputs.is_empty());
    }

    #[test]
    fn module_files_are_named_plainly() {
        for name in ["m.mod", "cactus_trial_m.mod", "parent@child.smod", "a1.mod"] {
            assert!(plain_module_name(name), "{name}");
        }
        for name in ["", ".mod", "m", "m.o", "../m.mod", "a/b.mod", ".hidden.mod", "m.mod.tmp", "a b.mod"] {
            assert!(!plain_module_name(name), "{name}");
        }
    }

    /// The real gfortran of this host, if it has one.
    pub(crate) fn gfortran() -> Option<PathBuf> {
        let found = super::super::identity::find_program(OsStr::new("gfortran")).ok()?;
        let says = Command::new(&found).arg("--version").output().ok()?;
        String::from_utf8_lossy(&says.stdout).contains("GNU Fortran").then_some(found)
    }

    /// A module and a source that uses it, keyed for real: what the compile
    /// reads and writes, as the dependency run says.
    #[test]
    fn keys_what_a_real_compile_reads_and_writes() {
        let Some(_) = gfortran() else {
            eprintln!("skipped: no gfortran on this host");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap().join("Cactus");
        let config = root.join("configs/sim");
        let (build, scratch, cc) = (config.join("build/T"), config.join("scratch"), config.join("cc"));
        for dir in [&build, &scratch, &cc] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(build.join("vals.inc"), "integer, parameter :: seven = 7\n").unwrap();
        std::fs::write(build.join("provider.f90"), "module provider\n  include 'vals.inc'\nend module provider\n").unwrap();
        // A module used by the source that defines it, with a stale module
        // file of that name in the working directory: not an input.
        std::fs::write(scratch.join("inner.mod"), "stale").unwrap();
        std::fs::write(
            build.join("user.f90"),
            "module inner\n  integer :: k\nend module inner\nmodule user\n  use inner\n  use provider\n  use, intrinsic :: iso_c_binding\ncontains\n  subroutine s()\n    print *, seven, k\n  end subroutine\nend module user\n",
        )
        .unwrap();
        let compiler = super::super::identity::identify(&cc, OsStr::new("gfortran")).unwrap();
        assert_eq!(compiler.family, Family::Gfortran);
        assert!(compiler.relocates, "gfortran compiles a source named from scratch alike in two trees");
        assert!(compiler.locale_neutral);
        let conf_map = PathMap::for_trial(&root, &config);
        let compile_of = |file: &str| {
            let args: Vec<OsString> = ["-g", "-c", "-o"].iter().map(OsString::from).chain([build.join(format!("{file}.o")).into(), build.join(file).into()]).collect();
            (super::super::compile::parse(&args).unwrap(), args)
        };
        let key_of = |compile: &Compile, args: &[OsString]| key(&compiler, OsStr::new("gfortran"), compile, args, &scratch, Some(&conf_map), &cc, true);
        // The provider writes `provider.mod` and reads the include beside it.
        let (compile, args) = compile_of("provider.f90");
        let provider = key_of(&compile, &args).unwrap();
        assert_eq!(provider.fortran.modules, ["provider.mod"]);
        assert_eq!(provider.fortran.renamed.as_ref().unwrap().given, Path::new("../build/T/provider.f90"));
        // Compiled for real, as the store's compile is: its module file is
        // there for the user.
        let argv: Vec<OsString> = provider.fortran.arguments(&args, compile.source_at);
        let status = Command::new("gfortran").args(&argv).args(conf_map.flags()).current_dir(&scratch).status().unwrap();
        assert!(status.success());
        assert!(scratch.join("provider.mod").is_file());
        drop(provider);
        // The user reads `provider.mod`, found in the working directory, and
        // keys it; a change to it is a change to the key.
        let (compile, args) = compile_of("user.f90");
        let keyed = key_of(&compile, &args).unwrap();
        assert_eq!(keyed.fortran.modules, ["inner.mod", "user.mod"]);
        let again = key_of(&compile, &args).unwrap();
        assert_eq!((&again.text, &again.files), (&keyed.text, &keyed.files));
        assert!(still_holds(&compiler, OsStr::new("gfortran"), Some(&conf_map), &keyed.fortran, &keyed.files, &keyed.seen));
        // The compile writes `inner.mod` over the stale one: no input changed.
        let argv: Vec<OsString> = keyed.fortran.arguments(&args, compile.source_at);
        assert!(Command::new("gfortran").args(&argv).args(conf_map.flags()).current_dir(&scratch).status().unwrap().success());
        assert!(still_holds(&compiler, OsStr::new("gfortran"), Some(&conf_map), &keyed.fortran, &keyed.files, &keyed.seen));
        std::fs::write(build.join("vals.inc"), "integer, parameter :: seven = 8\n").unwrap();
        let status = Command::new("gfortran").args(["-c", "-o"]).arg(build.join("provider.f90.o")).arg(build.join("provider.f90")).current_dir(&scratch).status().unwrap();
        assert!(status.success());
        let changed = key_of(&compile, &args).unwrap();
        assert_ne!(changed.files, keyed.files, "a module file read is keyed by its bytes");
        assert_eq!(changed.text, keyed.text);
        assert!(!still_holds(&compiler, OsStr::new("gfortran"), Some(&conf_map), &keyed.fortran, &keyed.files, &keyed.seen));
        // What keying leaves behind goes with the state.
        drop((keyed, again, changed));
        let left: Vec<_> = std::fs::read_dir(&cc).unwrap().map(|e| e.unwrap().file_name()).filter(|name| name.to_string_lossy().starts_with('.')).collect();
        assert!(left.is_empty(), "{left:?}");
    }

    /// A tree for the real-compiler tests: `<root>/configs/sim` with its
    /// `build/T`, `scratch` and `cc`, and a key for compiles of files in
    /// `build/T` from `scratch`, with `extra` flags.
    struct Real {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        build: PathBuf,
        scratch: PathBuf,
        cc: PathBuf,
        compiler: Compiler,
    }

    impl Real {
        fn new() -> Option<Self> {
            gfortran()?;
            let tmp = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(tmp.path()).unwrap().join("Cactus");
            let config = root.join("configs/sim");
            let (build, scratch, cc) = (config.join("build/T"), config.join("scratch"), config.join("cc"));
            for dir in [&build, &scratch, &cc] {
                std::fs::create_dir_all(dir).unwrap();
            }
            let compiler = super::super::identity::identify(&cc, OsStr::new("gfortran")).unwrap();
            Some(Self { _tmp: tmp, root, build, scratch, cc, compiler })
        }

        fn args(&self, file: &str, extra: &[&str]) -> Vec<OsString> {
            let mut args: Vec<OsString> = extra.iter().map(OsString::from).collect();
            args.extend([OsString::from("-c"), OsString::from("-o"), self.build.join(format!("{file}.o")).into(), self.build.join(file).into()]);
            args
        }

        fn key(&self, file: &str, extra: &[&str]) -> Result<Keyed, String> {
            let args = self.args(file, extra);
            let compile = super::super::compile::parse(&args).unwrap();
            let map = PathMap::for_trial(&self.root, &self.root.join("configs/sim"));
            key(&self.compiler, OsStr::new("gfortran"), &compile, &args, &self.scratch, Some(&map), &self.cc, true).map_err(|e| format!("{e:#}"))
        }

        /// Compile `source` (module files go to scratch), plainly.
        fn compile(&self, source: &str) {
            let status = Command::new("gfortran").args(["-c", "-o", "/dev/null"]).arg(self.build.join(source)).current_dir(&self.scratch).status().unwrap();
            assert!(status.success(), "{source}");
        }
    }

    /// What the preprocessor of the dependency run reads otherwise than the
    /// compile keeps the compile out: macros from flags, those `-fopenmp`
    /// defines, a backslash with blanks after it, a lone carriage return.
    #[test]
    fn a_source_the_preprocessor_reads_otherwise_is_not_cached() {
        let Some(real) = Real::new() else {
            eprintln!("skipped: no gfortran on this host");
            return;
        };
        std::fs::write(real.build.join("pm.f90"), "module pm\n  integer :: v = 1\nend module pm\n").unwrap();
        std::fs::write(real.build.join("pm_reentrant.f90"), "module pm_reentrant\n  integer :: v = 1\nend module pm_reentrant\n").unwrap();
        real.compile("pm.f90");
        real.compile("pm_reentrant.f90");
        // `-D` is no part of the compile, and none of the runs that
        // preprocess either.
        std::fs::write(real.build.join("other.f90"), "module other\n  integer :: v = 2\nend module other\n").unwrap();
        real.compile("other.f90");
        std::fs::write(real.build.join("d.f90"), "subroutine d()\n  use pm\n  print *, v\nend subroutine\n").unwrap();
        let keyed = real.key("d.f90", &["-Dpm=other"]).unwrap();
        assert!(keyed.fortran.args.iter().all(|arg| !arg.as_bytes().starts_with(b"-D")), "{:?}", keyed.fortran.args);
        drop(keyed);
        for (file, text, flags) in [
            ("omp.f", "      subroutine o()\n      use pm _REENTRANT\n      print *, v\n      end\n", &["-fopenmp"][..]),
            ("bs.f90", "subroutine b()\n  ! see C:\\  \n  use pm\n  print *, v\nend subroutine\n", &[][..]),
            ("cr.f90", "subroutine c()\n  ! remark\r#define pm other\n  use pm\n  print *, v\nend subroutine\n", &[][..]),
        ] {
            std::fs::write(real.build.join(file), text).unwrap();
            let err = real.key(file, flags).err().unwrap_or_default();
            assert!(err.contains("reads otherwise"), "{file}: {err}");
        }
        // Without the flag that defines the macro, it is a name like any.
        assert!(real.key("omp.f", &[]).is_ok());
    }

    /// Where the dependency run would find another file than the compile
    /// finds, the compile is not cached: a module file in the working
    /// directory and beside the source, which the compile finds in the
    /// working directory first; an included file in a directory below the
    /// working directory, which the compile does not search.
    #[test]
    fn a_file_found_elsewhere_than_the_compile_finds_it_is_not_cached() {
        let Some(real) = Real::new() else {
            eprintln!("skipped: no gfortran on this host");
            return;
        };
        std::fs::write(real.build.join("pm.f90"), "module pm\n  integer :: v = 1\nend module pm\n").unwrap();
        real.compile("pm.f90");
        std::fs::write(real.build.join("u.f90"), "subroutine u()\n  use pm\n  print *, v\nend subroutine\n").unwrap();
        assert!(real.key("u.f90", &[]).is_ok());
        std::fs::copy(real.scratch.join("pm.mod"), real.build.join("pm.mod")).unwrap();
        let err = real.key("u.f90", &[]).err().unwrap_or_default();
        assert!(err.contains("another module file"), "{err}");
        std::fs::remove_file(real.build.join("pm.mod")).unwrap();

        let thorn = real.root.join("arrangements/A/T/src");
        std::fs::create_dir_all(thorn.join("sub")).unwrap();
        std::fs::create_dir_all(real.scratch.join("sub")).unwrap();
        std::fs::write(thorn.join("sub/vals.inc"), "integer, parameter :: w = 2\n").unwrap();
        std::fs::write(real.build.join("i.f90"), "subroutine i()\n  include 'sub/vals.inc'\n  print *, w\nend subroutine\n").unwrap();
        let include = format!("-I{}", thorn.display());
        assert!(real.key("i.f90", &[&include]).is_ok());
        std::fs::write(real.scratch.join("sub/vals.inc"), "integer, parameter :: w = 1\n").unwrap();
        let err = real.key("i.f90", &[&include]).err().unwrap_or_default();
        assert!(err.contains("under the working directory"), "{err}");
    }

    /// Record mode writes no copy; a serving build's copy is checked again
    /// after the compile like any file read.
    #[test]
    fn the_copy_is_written_only_to_be_compiled_and_is_checked_after() {
        let Some(real) = Real::new() else {
            eprintln!("skipped: no gfortran on this host");
            return;
        };
        let original = real.root.join("arrangements/A/T/src/x.F90");
        std::fs::write(real.build.join("x.f90"), format!("# 1 \"{}\"\nsubroutine x()\n  print *, 1\nend subroutine\n", original.display())).unwrap();
        let args = real.args("x.f90", &[]);
        let compile = super::super::compile::parse(&args).unwrap();
        let map = PathMap::for_trial(&real.root, &real.root.join("configs/sim"));
        let recorded = key(&real.compiler, OsStr::new("gfortran"), &compile, &args, &real.scratch, Some(&map), &real.cc, false).unwrap();
        assert!(!real.build.join(".cactup").exists(), "record mode wrote a copy");
        let served = real.key("x.f90", &[]).unwrap();
        assert_eq!((&recorded.text, &recorded.files), (&served.text, &served.files), "recorded and served alike");
        let copy = real.build.join(".cactup/x.f90");
        assert!(copy.is_file());
        assert!(still_holds(&real.compiler, OsStr::new("gfortran"), Some(&map), &served.fortran, &served.files, &served.seen));
        std::fs::write(&copy, "subroutine x()\n  print *, 2\nend subroutine\n").unwrap();
        assert!(!still_holds(&real.compiler, OsStr::new("gfortran"), Some(&map), &served.fortran, &served.files, &served.seen));
    }

    /// A text file named like a module file is an included file, with an
    /// included file's rules; an included file of a name the copy's
    /// directory has keeps a copied source out, wherever it was found.
    #[test]
    fn included_files_are_told_by_what_they_are_and_where_the_compile_looks() {
        let Some(real) = Real::new() else {
            eprintln!("skipped: no gfortran on this host");
            return;
        };
        let shared = real.root.join("shared");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join("cfg.mod"), "integer, parameter :: z = 2\n").unwrap();
        std::fs::write(real.build.join("t.f90"), "subroutine t()\n  include 'cfg.mod'\n  print *, z\nend subroutine\n").unwrap();
        let include = format!("-I{}", shared.display());
        assert!(real.key("t.f90", &[&include]).is_ok());
        std::fs::write(real.scratch.join("cfg.mod"), "integer, parameter :: z = 1\n").unwrap();
        let err = real.key("t.f90", &[&include]).err().unwrap_or_default();
        assert!(err.contains("under the working directory"), "{err}");

        let original = real.root.join("arrangements/A/T/src/c.F90");
        std::fs::write(shared.join("b.f90"), "integer, parameter :: y = 6\n").unwrap();
        std::fs::write(real.build.join("c.f90"), format!("# 1 \"{}\"\nsubroutine c()\n  include 'b.f90'\n  print *, y\nend subroutine\n", original.display())).unwrap();
        assert!(real.key("c.f90", &[&include]).is_ok());
        std::fs::write(real.build.join(".cactup/b.f90"), "integer, parameter :: y = 5\n").unwrap();
        let err = real.key("c.f90", &[&include]).err().unwrap_or_default();
        assert!(err.contains("beside the copy"), "{err}");
    }

    /// A module file that is not compressed is still a module file to
    /// gfortran, and is checked as one: here one beside the source, which
    /// the compile finds after the working directory's. And a copied
    /// source's included file named with `..` keeps the compile out.
    #[test]
    fn module_files_by_name_and_includes_by_the_name_as_written() {
        let Some(real) = Real::new() else {
            eprintln!("skipped: no gfortran on this host");
            return;
        };
        std::fs::write(real.build.join("mymod.f90"), "module mymod\n  integer :: v = 1\nend module mymod\n").unwrap();
        real.compile("mymod.f90");
        std::fs::write(real.build.join("user.f90"), "subroutine user()\n  use mymod\n  print *, v\nend subroutine\n").unwrap();
        let plain = Command::new("gzip").arg("-dc").arg(real.scratch.join("mymod.mod")).output().unwrap();
        assert!(plain.status.success());
        std::fs::write(real.build.join("mymod.mod"), &plain.stdout).unwrap();
        let err = real.key("user.f90", &[]).err().unwrap_or_default();
        assert!(err.contains("another module file"), "{err}");
        std::fs::remove_file(real.build.join("mymod.mod")).unwrap();

        let sub = real.build.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(real.build.join("x.inc"), "integer, parameter :: z = 2\n").unwrap();
        let original = real.root.join("arrangements/A/T/src/sub/a.F90");
        std::fs::write(sub.join("a.f90"), format!("# 1 \"{}\"\nsubroutine a()\n  include '../x.inc'\n  print *, z\nend subroutine\n", original.display())).unwrap();
        let args = real.args("sub/a.f90", &[]);
        let compile = super::super::compile::parse(&args).unwrap();
        let map = PathMap::for_trial(&real.root, &real.root.join("configs/sim"));
        let err = key(&real.compiler, OsStr::new("gfortran"), &compile, &args, &real.scratch, Some(&map), &real.cc, true).err().map(|e| format!("{e:#}")).unwrap_or_default();
        assert!(err.contains("\"..\""), "{err}");
    }
}
