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
//! does not run, so the source must be one the preprocessor reads as gfortran
//! does ([`check_text`]). It writes module files, and reads back the ones it
//! wrote, so it runs in an empty directory of its own with the compile's
//! working directory first among its `-I` directories: it then finds every
//! module file where the compile finds it, its own first.
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

/// Is `text` a source the C preprocessor of the dependency run reads as
/// gfortran reads it? It runs in traditional mode with every macro it can
/// drop dropped (`-undef`); what is left that it would act on: a comment
/// (`/*`, which in traditional mode swallows lines up to the next `*/`, code
/// included), a directive other than a line marker, a line joined to the next
/// (`\` at its end), and a name it still defines (`__FILE__`, `__GFC_INT_8__`:
/// every one of them begins and ends with two underscores; a word that only
/// begins so, such as the rest of a name continued on a fixed-form line,
/// `&__lambda`, is defined by nothing). Anything of it makes the source one
/// whose dependencies cannot be had this way, and the compile is not cached.
pub fn check_text(text: &[u8]) -> Result<(), String> {
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    for line in text.split(|b| *b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.first() == Some(&b'#') && !is_marker(line)? {
            return Err(format!("the source has a preprocessor line ({}), which the cache does not follow", String::from_utf8_lossy(line)));
        }
        if line.ends_with(b"\\") {
            return Err("the source has a line ending in a backslash, which the dependency run would join to the next".to_owned());
        }
        if line.windows(2).any(|pair| pair == b"/*") {
            return Err("the source has \"/*\", which the dependency run would take for the start of a comment".to_owned());
        }
        let defined = line.windows(2).enumerate().any(|(at, pair)| {
            let word = &line[at..at + line[at..].iter().take_while(|b| ident(**b)).count()];
            pair == b"__" && (at == 0 || !ident(line[at - 1])) && word.len() > 4 && word.ends_with(b"__")
        });
        if defined {
            return Err("the source has a name such as the dependency run's preprocessor defines (__NAME__)".to_owned());
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
    /// The file compiled, by its physical path: the source, or the copy.
    compiled: PathBuf,
    /// Whether that is a copy, made to map the source's line markers.
    copied: bool,
    /// The source's own directory, by its physical path.
    source_dir: PathBuf,
    /// The source as the recipe named it.
    original: OsString,
}

impl Renamed {
    /// How the compile of `source` (as the recipe named it, read as `text`)
    /// in `cwd` names it under `map`: by its path from `cwd`, and if `text`
    /// has line markers that name files, as a copy with those mapped,
    /// written into `.cactup/` beside the source. The copy is written whole
    /// (a temporary file renamed into place) and left there, like the build
    /// copy itself: another compile of the same source makes the same one.
    pub fn new(cwd: &Path, source: &Path, text: &[u8], map: &PathMap) -> Res<Self> {
        let cwd = std::fs::canonicalize(cwd).with_context(|| format!("Failed to resolve {}", cwd.display()))?;
        let joined = cwd.join(source);
        let name = joined.file_name().with_context(|| format!("{} names no file", source.display()))?;
        let dir = joined.parent().unwrap_or(Path::new("/"));
        let source_dir = std::fs::canonicalize(dir).with_context(|| format!("Failed to resolve {}", dir.display()))?;
        let (compiled, copied) = match names_files(text).map_err(anyhow::Error::msg)? {
            false => (source_dir.join(name), false),
            true => {
                let mapped = mapped_text(text, map).map_err(anyhow::Error::msg)?;
                let copies = source_dir.join(".cactup");
                std::fs::create_dir_all(&copies).with_context(|| format!("Failed to create {}", copies.display()))?;
                let mut temp = tempfile::Builder::new()
                    .prefix(".copy-")
                    .tempfile_in(&copies)
                    .with_context(|| format!("Failed to create a file in {}", copies.display()))?;
                std::io::Write::write_all(&mut temp, &mapped).context("Failed to write a copy of the source")?;
                let copy = copies.join(name);
                temp.persist(&copy).with_context(|| format!("Failed to write {}", copy.display()))?;
                (copy, true)
            }
        };
        Ok(Self { given: relative(&cwd, &compiled), compiled, copied, source_dir, original: source.as_os_str().to_owned() })
    }

    /// The include flag that has gfortran look in the source's directory
    /// first, as it would have for the source itself (only a copy needs it).
    fn include_flag(&self) -> Option<OsString> {
        self.copied.then(|| {
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
    /// The module files the compile writes.
    pub modules: Vec<String>,
    /// The dependency runs' working directory, where they write their
    /// module files.
    private: tempfile::TempDir,
    /// The arguments of the dependency run.
    args: Vec<OsString>,
    /// The file the dependency run reads, by its absolute path.
    compiled: PathBuf,
    /// The source's directory, when the compile searches it only because it
    /// reads a copy.
    source_dir_added: Option<PathBuf>,
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

/// Key what a Fortran `compile` reads (§18.10), given by the arguments
/// `args` (the command line without the program) and run in `cwd`. `in_dir`
/// is a directory of the build's for the dependency runs.
pub fn key(compiler: &Compiler, name: &OsStr, compile: &Compile, args: &[OsString], cwd: &Path, map: Option<&PathMap>, in_dir: &Path) -> Res<Keyed> {
    if compiler.family != Family::Gfortran {
        bail!("Fortran is cached for gfortran only");
    }
    // The dependency run runs elsewhere: a directory it is given by a
    // relative name would be another directory there.
    let mut flags = compile.preprocess.iter();
    while let Some(flag) = flags.next() {
        if flag == "-I" && flags.next().is_some_and(|dir| Path::new(dir).is_relative()) {
            bail!("an include directory is named by a relative path");
        }
    }
    let cwd = std::fs::canonicalize(cwd).with_context(|| format!("Failed to resolve {}", cwd.display()))?;
    // The source, read once: what the compile reads is this, or a copy of it.
    let source = cwd.join(&compile.source);
    let mut file = std::fs::File::open(&source).with_context(|| format!("Failed to open {}", source.display()))?;
    let mut text = Vec::new();
    file.read_to_end(&mut text).with_context(|| format!("Failed to read {}", source.display()))?;
    check_text(&text).map_err(anyhow::Error::msg)?;
    let renamed = map.map(|map| Renamed::new(&cwd, &compile.source, &text, map)).transpose()?;
    let (compiled, given) = match &renamed {
        Some(renamed) if renamed.copied => {
            (std::fs::read(&renamed.compiled).with_context(|| format!("Failed to read {}", renamed.compiled.display()))?, renamed.given.clone())
        }
        Some(renamed) => (text, renamed.given.clone()),
        None => (text, compile.source.clone()),
    };
    let private = tempfile::Builder::new()
        .prefix(".fortran-")
        .tempdir_in(in_dir)
        .with_context(|| format!("Failed to create a directory in {}", in_dir.display()))?;
    let compiled_path = renamed.as_ref().map_or_else(|| source.clone(), |renamed| renamed.compiled.clone());
    let source_dir_added = match &renamed {
        Some(renamed) if renamed.copied && !searched_anyway(compile, &cwd, &renamed.source_dir) => Some(renamed.source_dir.clone()),
        _ => None,
    };
    // The dependency run: in its own directory, finding module files in the
    // compile's working directory after its own; the file the compile
    // reads, by its absolute path; the compile's arguments otherwise.
    let (without_output, source_at) = compile.without_output(args);
    let mut run_args = without_output;
    run_args[source_at] = compiled_path.as_os_str().to_owned();
    if let Some(flag) = renamed.as_ref().and_then(Renamed::include_flag) {
        run_args.insert(0, flag);
    }
    let mut first = OsString::from("-I");
    first.push(&cwd);
    run_args.insert(0, first);
    run_args.extend(["-cpp", "-undef", "-M", "-fsyntax-only", "-v"].map(OsString::from));
    let mut fortran = Fortran { renamed, modules: Vec::new(), private, args: run_args, compiled: compiled_path, source_dir_added, cwd };

    let deps = dependencies(compiler, name, &fortran)?;
    fortran.modules = deps.modules.clone();
    let mut hasher = Hasher::new("fortran text");
    hasher.feed(given.as_os_str().as_bytes());
    hasher.feed(&compiled);
    for module in &deps.modules {
        hasher.feed(module.as_bytes());
    }
    let (files, seen, count) = read_inputs(&deps, map, &fortran)?;
    Ok(Keyed { text: hasher.hex(), text_bytes: compiled.len() as u64, files, count, seen, fortran })
}

/// Do the files `fortran`'s dependency run lists still say what they said
/// (`files`, `seen`), and does it still list the same?
pub fn still_holds(compiler: &Compiler, name: &OsStr, map: Option<&PathMap>, fortran: &Fortran, files: &str, seen: &str) -> bool {
    let Ok(deps) = dependencies(compiler, name, fortran) else { return false };
    deps.modules == fortran.modules
        && read_inputs(&deps, map, fortran).is_ok_and(|(now_files, now_seen, _)| now_files == files && now_seen == seen)
}

/// The files the dependency run listed, digested as `key::read_files` does,
/// with the source itself when the compile reads it and not a copy (its
/// bytes are then checked again after the compile, like any file read).
fn read_inputs(deps: &Dependencies, map: Option<&PathMap>, fortran: &Fortran) -> Res<(String, String, usize)> {
    let mut named = Named::new();
    let mapped = |path: &Path| map.map_or_else(|| path.as_os_str().as_bytes().to_vec(), |map| map.apply(path.as_os_str().as_bytes()));
    for input in &deps.inputs {
        let module = input.extension().is_some_and(|ext| ext == "mod" || ext == "smod");
        let dir = input.parent();
        if module && fortran.source_dir_added.as_deref().is_some_and(|added| dir == Some(added)) {
            bail!("a module file beside the source would be read, which the compile without the cache would not find");
        }
        // The dependency run searches the working directory for included
        // files too; the compile does not.
        if !module && dir == Some(fortran.cwd.as_path()) {
            bail!("an included file would be found in the working directory, where the compile does not look for one");
        }
        named.insert((mapped(input), input.clone()), true);
    }
    let copied = fortran.renamed.as_ref().is_some_and(|renamed| renamed.copied);
    if !copied {
        named.insert((b"source".to_vec(), fortran.compiled.clone()), true);
    }
    let (files, seen) = key::read_files(&named, map)?;
    Ok((files, seen, named.len()))
}

/// Does the compile itself, run in `cwd`, look for files in `dir` (as one of
/// its own `-I` directories)?
fn searched_anyway(compile: &Compile, cwd: &Path, dir: &Path) -> bool {
    let same = |other: &Path| std::fs::canonicalize(cwd.join(other)).is_ok_and(|other| other == dir);
    let args = &compile.preprocess;
    args.iter().zip(args.iter().skip(1)).any(|(flag, value)| flag == "-I" && same(Path::new(value)))
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
    let copies = fortran.renamed.as_ref().filter(|renamed| renamed.copied).map(|renamed| renamed.source_dir.join(".cactup"));
    parse_rule(&out.stdout, &fortran.compiled, copies.as_deref()).map_err(anyhow::Error::msg)
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
    fn a_source_the_dependency_run_reads_otherwise_is_not_cached() {
        let fine = "module m\n  integer :: multi_BH__m = 1 ! a name with two underscores inside\n  character(*), parameter :: s = 'p' // 'q'\nend module m\n      x = Lemaitre\n     &__lambda\n";
        assert_eq!(check_text(fine.as_bytes()), Ok(()));
        assert_eq!(check_text(b"# 1 \"/w/Cactus/arrangements/A/T/src/x.F90\"\nmodule m\n# 7\nend module m\n"), Ok(()));
        for (text, why) in [
            ("x = 1 ! /* not a comment to Fortran\n", "/*"),
            ("x = 1 \\\ny = 2\n", "backslash"),
            ("#define X 1\nx = X\n", "preprocessor line"),
            ("#include \"a.h\"\n", "preprocessor line"),
            ("#line 1 \"a.f90\"\n", "preprocessor line"),
            ("print *, __FILE__\n", "__NAME__"),
            ("__GFC_INT_8__ = 1\n", "__NAME__"),
            ("x = (__LINE__)\n", "__NAME__"),
        ] {
            let err = check_text(text.as_bytes()).unwrap_err();
            assert!(err.contains(why), "{text:?}: {err}");
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
            let renamed = Renamed::new(&scratch, &source, text.as_bytes(), &map).unwrap();
            let copy = renamed.copied.then(|| std::fs::read_to_string(&renamed.compiled).unwrap());
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
        let key_of = |compile: &Compile, args: &[OsString]| key(&compiler, OsStr::new("gfortran"), compile, args, &scratch, Some(&conf_map), &cc);
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
}
