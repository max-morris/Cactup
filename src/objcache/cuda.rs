//! CUDA compiles by NVIDIA's `nvcc` (M4, `design/build-cache/M4-CUDA.md`).
//!
//! `nvcc` is a driver of its own: for one `-c` it runs the host compiler's
//! preprocessor twice (the host pass, and the device pass with
//! `__CUDA_ARCH__` defined), `cudafe++`, `cicc`, `ptxas`, `fatbinary`, and
//! the host compiler for the object. It says what it would run, and in what
//! environment (from its `nvcc.profile`), without running it: `-dryrun`.
//! The identity is made from that ([`DryRun`]: its programs, found as `nvcc`
//! finds them, and its `libdevice`), the key from the two preprocessor
//! passes it would run, run here.

use super::compile::Compile;
use super::hash::{file_digest, Hasher};
use super::identity::{self, Compiler, Family, Seen, Watched};
use super::key::{depend_files, diagnosed_files, digest_output, flags_from_elsewhere, read_files, reads_match, said_as_compiled, said_of_the_text, Named, Parts, PathMap, Used, PREPROCESSOR};
use crate::Res;
use anyhow::{bail, Context};
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;

/// What `nvcc -dryrun` says: the environment it sets for the programs it
/// runs (`PATH`, `LD_LIBRARY_PATH`, `CICC_PATH`, `NVVMIR_LIBRARY_DIR`, …),
/// the commands, each as its words, and what `nvcc` says itself (its
/// warnings: the lines that are not commands).
#[derive(Debug, Default, PartialEq)]
pub struct DryRun {
    pub env: Vec<(String, String)>,
    pub commands: Vec<Vec<String>>,
    pub said: Vec<String>,
    /// A command that is more shell than [`shell_words`] follows: why.
    pub unsupported: Option<String>,
}

impl DryRun {
    /// Read what `nvcc -dryrun` wrote (to standard error): each line it
    /// would run is `#$ ` and the line. An assignment (`NAME=value`) sets the
    /// environment of what follows; any other line is a command, which
    /// `nvcc` hands to `sh -c` (seen), split as the shell would split it,
    /// with `$NAME` taken from that environment, else this process's.
    pub fn read(said: &str) -> Self {
        let mut run = Self::default();
        for line in said.lines() {
            let Some(line) = line.strip_prefix("#$ ") else {
                run.said.push(line.to_owned());
                continue;
            };
            let name_end = line.find('=').filter(|at| {
                let name = &line[..*at];
                !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            });
            match name_end {
                Some(at) => {
                    // As written: `_SPACE_= ` is one blank.
                    let value = &line[at + 1..];
                    run.env.retain(|(name, _)| name != &line[..at]);
                    run.env.push((line[..at].to_owned(), value.to_owned()));
                }
                None => match shell_words(line, |name| run.get(name).map(str::to_owned).or_else(|| std::env::var(name).ok())) {
                    Ok(words) if !words.is_empty() => run.commands.push(words),
                    Ok(_) => {}
                    Err(why) => {
                        run.unsupported.get_or_insert(why);
                    }
                },
            }
        }
        run
    }

    /// What decides which programs `nvcc` runs and how they find their
    /// own: the programs, and the environment it gives them.
    pub fn toolchain(&self) -> (Vec<&str>, Vec<&(String, String)>) {
        (self.programs(), self.env.iter().collect())
    }

    /// The value `nvcc` gives `name`, if it gives one.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.env.iter().rev().find(|(set, _)| set == name).map(|(_, value)| value.as_str())
    }

    /// The commands that run the host compiler's preprocessor (`-E`): the
    /// host pass and the device passes, in the order `nvcc` runs them.
    pub fn preprocessor_passes(&self) -> impl Iterator<Item = &Vec<String>> {
        self.commands.iter().filter(|words| words.iter().skip(1).any(|word| word == "-E"))
    }

    /// The programs of `nvcc`'s own toolchain it runs (not the host
    /// compiler, not `rm`), by the names it runs them by.
    pub fn programs(&self) -> Vec<&str> {
        let mut programs: Vec<&str> = self
            .commands
            .iter()
            .filter_map(|words| words.first())
            .map(String::as_str)
            .filter(|program| {
                let base = program.rsplit('/').next().unwrap_or(program);
                TOOLCHAIN.contains(&base)
            })
            .collect();
        programs.sort_unstable();
        programs.dedup();
        programs
    }
}

/// `line` split into words as a POSIX shell splits it: blanks separate,
/// double and single quotes join, a backslash escapes (inside double quotes
/// only `$`, `` ` ``, `"`, `\\` and a line break), `$NAME` and `${NAME}` are
/// replaced by what `get` gives (outside single quotes), and an unquoted
/// expansion that comes to nothing is no word. `Err`: shell syntax this does
/// not follow (a command substitution, an operator, a pattern, `~`, another
/// form of `${…}`).
pub fn shell_words(line: &str, get: impl Fn(&str) -> Option<String>) -> Result<Vec<String>, String> {
    let unsupported = |what: &str| Err(format!("nvcc would run a command with {what} in it, which cactup does not repeat"));
    let mut words = Vec::new();
    let (mut word, mut any) = (String::new(), false);
    let mut quote: Option<char> = None;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some('\''), '\'') | (Some('"'), '"') => quote = None,
            (Some('\''), c) => word.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                any = true;
            }
            (Some('"'), '\\') => match chars.peek() {
                Some(&next @ ('$' | '`' | '"' | '\\' | '\n')) => {
                    chars.next();
                    word.push(next);
                }
                _ => word.push('\\'),
            },
            (_, '\\') => {
                if let Some(next) = chars.next() {
                    word.push(next);
                    any = true;
                }
            }
            (_, '`') => return unsupported("a command substitution"),
            (_, '$') => {
                let braced = chars.peek() == Some(&'{');
                if braced {
                    chars.next();
                }
                let mut name = String::new();
                while let Some(&next) = chars.peek() {
                    if next.is_ascii_alphanumeric() || next == '_' {
                        name.push(next);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if braced && chars.next() != Some('}') {
                    return unsupported("a form of ${…}");
                }
                if name.is_empty() {
                    match chars.peek() {
                        Some('(') => return unsupported("a command substitution"),
                        Some('@' | '*' | '#' | '?' | '-' | '$' | '!') => return unsupported("a special parameter"),
                        _ => {
                            word.push('$');
                            any = true;
                        }
                    }
                } else {
                    let value = get(&name).unwrap_or_default();
                    // The shell would split it at blanks, and expand a
                    // pattern in it.
                    if quote.is_none() && value.contains(|c: char| c.is_whitespace() || "*?[".contains(c)) {
                        return unsupported("a value of $NAME with blanks or a pattern in it, unquoted");
                    }
                    any |= !value.is_empty() || quote.is_some();
                    word.push_str(&value);
                }
            }
            (None, c) if c.is_whitespace() => {
                if any {
                    words.push(std::mem::take(&mut word));
                }
                any = false;
            }
            (None, ';' | '|' | '&' | '<' | '>' | '(' | ')') => return unsupported("a shell operator"),
            (None, '*' | '?' | '[') => return unsupported("a pattern"),
            (None, '~') if !any => return unsupported("a ~"),
            (None, '#') if !any => break,
            (_, c) => {
                word.push(c);
                any = true;
            }
        }
    }
    if quote.is_some() {
        return unsupported("an unclosed quote");
    }
    if any {
        words.push(word);
    }
    Ok(words)
}

/// What `nvcc` (at `path`) would run for `args`, in the working directory
/// `dir` where one is given.
pub fn dry_run(path: &Path, name: &OsStr, args: &[OsString], dir: Option<&Path>) -> Res<DryRun> {
    let mut command = Command::new(path);
    command.arg0(name).arg("-dryrun").args(args).stdin(Stdio::null()).stdout(Stdio::null());
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    super::key::in_english(&mut command);
    let out = command.output().with_context(|| format!("Failed to run {} -dryrun", path.display()))?;
    let run = DryRun::read(&String::from_utf8_lossy(&out.stderr));
    if let Some(why) = &run.unsupported {
        bail!("{why}");
    }
    if run.commands.is_empty() {
        bail!("{} -dryrun does not say what it would run", path.display());
    }
    Ok(run)
}

/// The arguments of a dry run that says what `nvcc` runs for any compile.
fn plain_compile() -> Vec<OsString> {
    ["-x", "cu", "-c", "/dev/null", "-o", "/dev/null"].map(OsString::from).to_vec()
}

/// The part of `nvcc`'s identity that is its own (`identity::examine` has
/// its bytes and `--version` already): its profile, which sets the
/// environment of what it runs; the programs it runs (`cudafe++`, `cicc`,
/// `ptxas`, `fatbinary`), each found as `nvcc` finds it (along the `PATH` it
/// gives them, the places passed over watched) and to be listed with the
/// libraries it loads in the library path it gives them (`programs`); and
/// its device library (`libdevice`, every file in the directory it names).
/// Asked again after the looking, it must say the same.
pub(super) fn examine(path: &Path, driver: &Path, hasher: &mut Hasher, watched: &mut Watched, programs: &mut Vec<(PathBuf, Option<String>)>) -> Res<()> {
    let profile = driver.with_file_name("nvcc.profile");
    watched.files.push(Seen::of(&profile)?);
    hasher.feed(b"nvcc.profile");
    hasher.feed(&fs::read(&profile).with_context(|| format!("Failed to read {}", profile.display()))?);
    let name = path.file_name().unwrap_or(path.as_os_str());
    let run = dry_run(path, name, &plain_compile(), None)?;
    let search = run.get("PATH").with_context(|| format!("{} does not say where it finds its programs", path.display()))?;
    let library_path = run.get("LD_LIBRARY_PATH").map(str::to_owned);
    for program in run.programs() {
        let base = program.rsplit('/').next().unwrap_or(program);
        let found = identity::search_along(OsStr::new(program), OsStr::new(search), watched)?
            .with_context(|| format!("{} runs {base}, which is not where it looks", path.display()))?;
        watched.files.extend(Seen::of(&found));
        let file = fs::canonicalize(&found).with_context(|| format!("Failed to resolve {}", found.display()))?;
        // As for GCC's programs: its own bytes must say what it is.
        let marker = if base == "cudafe++" { "cudafe" } else { base };
        let bytes = fs::read(&file).with_context(|| format!("Failed to read {}", file.display()))?;
        if !bytes.starts_with(b"\x7fELF") || !bytes.windows(marker.len()).any(|window| window == marker.as_bytes()) {
            bail!("{} runs {} for {base}, which is no program cactup can identify (a script or a wrapper, maybe)", path.display(), file.display());
        }
        hasher.feed(base.as_bytes());
        hasher.feed(file_digest(&file)?.as_bytes());
        watched.files.push(Seen::of(&file)?);
        programs.push((file, library_path.clone()));
    }
    let libdevice = run.get("NVVMIR_LIBRARY_DIR").with_context(|| format!("{} does not say where its device library is", path.display()))?;
    let dir = fs::canonicalize(libdevice).with_context(|| format!("Failed to resolve {libdevice}"))?;
    // The directory too: a file added to it changes it.
    watched.files.push(Seen::of(&dir)?);
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .with_context(|| format!("Failed to read {}", dir.display()))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<_, _>>()
        .with_context(|| format!("Failed to read {}", dir.display()))?;
    files.sort();
    for file in files {
        hasher.feed(file.file_name().unwrap_or_default().as_bytes());
        hasher.feed(file_digest(&file)?.as_bytes());
        watched.files.push(Seen::of(&file)?);
    }
    // (Not word for word: without `--objdir-as-tempdir` its intermediate
    // files have names of their own each time, `tmpxft_…`; seen.)
    if dry_run(path, name, &plain_compile(), None)?.toolchain() != run.toolchain() {
        bail!("{} would run other programs than it said a moment ago", path.display());
    }
    Ok(())
}

/// A CUDA compile as the cache runs it (decision 25): what `nvcc` would
/// run for it, run here, step by step, with the module ID given rather than
/// made. `cudafe++` makes a translation unit's module ID, which names its
/// registration, its fat binary and every entity of internal linkage, from
/// the name of a routine or variable it defines where one serves; where none
/// does, from the working directory, the time its source was last changed (as
/// local time) and its process ID (all seen on qbd, decision 25). Not told to
/// make one (`--gen_module_id_file`), it reads the one in the file it is
/// given, as `cicc` does after it (tried); nothing else of the compile
/// changes.
#[derive(Debug)]
pub struct Plan {
    env: Vec<(String, String)>,
    steps: Vec<Step>,
    /// The file the module ID is read from, and the ID.
    id_file: PathBuf,
    id: String,
    /// What `nvcc` says itself before it runs anything (its warnings).
    said: Vec<u8>,
    /// The last step that runs the host compiler: the compile of the code
    /// `nvcc` generates, into the object. It is given `-MD -MF <reads>`, to
    /// say what it read (`identity::Compiler::host_reads`).
    last: usize,
    reads: PathBuf,
    /// The preprocessing steps (`-E`), each given `-MD -MF <file>` too: what
    /// they read must be what the key's passes read (decision 28).
    pass_reads: Vec<(usize, PathBuf)>,
    /// The plan has run (record mode runs `nvcc` as the recipe asks).
    ran: std::cell::Cell<bool>,
}

/// One line of what `nvcc` would run.
#[derive(Debug)]
pub enum Step {
    /// A program, by the name `nvcc` runs it by, and its arguments; and the
    /// file that is (the one identified, once [`Plan::resolve`] has found
    /// it along the `PATH` `nvcc` gives).
    Run { words: Vec<String>, file: Option<PathBuf> },
    /// An intermediate file `nvcc` removes.
    Remove(PathBuf),
}

/// The programs of `nvcc`'s own toolchain, by their base names.
const TOOLCHAIN: &[&str] = &["cudafe++", "cicc", "ptxas", "fatbinary", "nvlink", "bin2c"];

impl Plan {
    /// What `nvcc` (at `path`, run as `name`) would run for `args` in the
    /// working directory, with the module ID still to be given
    /// ([`Self::give_id`]).
    pub fn new(path: &Path, name: &OsStr, args: &[OsString]) -> Res<Self> {
        Self::from_dry_run(dry_run(path, name, args, None)?, String::new()).with_context(|| format!("{} would compile in a way cactup cannot repeat", path.display()))
    }

    fn from_dry_run(run: DryRun, id: String) -> Res<Self> {
        if let Some(why) = run.unsupported {
            bail!("{why}");
        }
        let mut steps = Vec::new();
        let mut id_file = None;
        let mut last = None;
        let mut pass_steps = Vec::new();
        for mut words in run.commands {
            let base = words[0].rsplit('/').next().unwrap_or(&words[0]).to_owned();
            if base == "rm" {
                let [_, file] = words.as_slice() else { bail!("it would run rm otherwise than on one file") };
                steps.push(Step::Remove(PathBuf::from(file)));
                continue;
            }
            if base == "cudafe++" {
                let made = words.iter().filter(|word| *word == "--gen_module_id_file").count();
                let file = words.iter().position(|word| word == "--module_id_file_name").and_then(|at| words.get(at + 1)).cloned();
                match (made, file) {
                    (1, Some(file)) if id_file.is_none() => id_file = Some(PathBuf::from(file)),
                    _ => bail!("its cudafe++ would not make one module ID in a file"),
                }
                words.retain(|word| word != "--gen_module_id_file");
            }
            if !TOOLCHAIN.contains(&base.as_str()) && words.iter().any(|word| word == "-c") {
                last = Some(steps.len());
            }
            if !TOOLCHAIN.contains(&base.as_str()) && words.iter().any(|word| word == "-E") {
                pass_steps.push(steps.len());
            }
            steps.push(Step::Run { words, file: None });
        }
        let id_file = id_file.context("it would run no cudafe++ that makes a module ID")?;
        let last = last.context("it would run no host compile of what it generates")?;
        // Short names of this process's own beside the object (one made of
        // the object's could pass the longest name a directory takes).
        let dir = id_file.parent().map(Path::to_owned).unwrap_or_default();
        let reads = dir.join(format!(".cactup-{}-last.d", std::process::id()));
        let said = run.said.iter().flat_map(|line| [line.as_bytes(), b"\n"].concat()).collect();
        let pass_reads = pass_steps
            .into_iter()
            .enumerate()
            .map(|(n, at)| (at, dir.join(format!(".cactup-{}-pass{n}.d", std::process::id()))))
            .collect();
        Ok(Self { env: run.env, steps, id_file, id, said, last, reads, pass_reads, ran: std::cell::Cell::new(false) })
    }

    /// Find each program the plan runs along the `PATH` `nvcc` gives it, as
    /// `sh` would: each must be the host compiler `host` or one of `nvcc`'s
    /// own programs as its identity found them (`toolchain`), and that file
    /// is what runs.
    pub fn resolve(&mut self, host: &Compiler, toolchain: &[PathBuf]) -> Res<()> {
        let search = self.get("PATH").map(OsString::from).or_else(|| std::env::var_os("PATH")).unwrap_or_default();
        let host_file = fs::canonicalize(&host.path).with_context(|| format!("Failed to resolve {}", host.path.display()))?;
        for step in &mut self.steps {
            let Step::Run { words, file } = step else { continue };
            let program = OsStr::new(&words[0]);
            let found = match program.as_bytes().contains(&b'/') {
                true => Some(PathBuf::from(program)),
                false => identity::find_along(program, &search)?,
            };
            let found = found.with_context(|| format!("nvcc would run {}, which is not on its PATH", words[0]))?;
            let real = fs::canonicalize(&found).with_context(|| format!("Failed to resolve {}", found.display()))?;
            if real != host_file && !toolchain.contains(&real) {
                bail!("nvcc would run {}, which is neither its host compiler nor a program of its own it was identified with", found.display());
            }
            *file = Some(found);
        }
        Ok(())
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.env.iter().rev().find(|(set, _)| set == name).map(|(_, value)| value.as_str())
    }

    /// The module ID to give.
    pub fn set_id(&mut self, id: String) {
        self.id = id;
    }

    /// Put the module ID where `cudafe++` will read it: the plan is about to
    /// run.
    pub fn give_id(&self) -> std::io::Result<()> {
        self.ran.set(true);
        fs::write(&self.id_file, &self.id)
    }

    /// Has the plan run (since [`Self::give_id`])?
    pub fn ran(&self) -> bool {
        self.ran.get()
    }

    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    /// What `nvcc` says itself before it runs anything.
    pub fn said(&self) -> &[u8] {
        &self.said
    }

    /// The plan as the key has it: every step's words, every file it
    /// removes, and the environment `nvcc` gives them (but `PATH` and
    /// `LD_LIBRARY_PATH`, whose programs and libraries the identities
    /// follow), each name in them as `mapped` gives it. Flags `nvcc` takes
    /// from the environment (`PTXAS_FLAGS`, `CUDAFE_FLAGS`, through its
    /// profile) are in the words.
    pub fn described(&self, mapped: impl Fn(&[u8]) -> Vec<u8>) -> Vec<u8> {
        let mut out = Vec::new();
        let mut put = |bytes: &[u8]| {
            out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
            out.extend_from_slice(bytes);
        };
        for (name, value) in self.env.iter().filter(|(name, _)| name != "PATH" && name != "LD_LIBRARY_PATH") {
            put(name.as_bytes());
            put(&mapped(value.as_bytes()));
        }
        for step in &self.steps {
            match step {
                Step::Run { words, .. } => {
                    put(b"run");
                    words.iter().for_each(|word| put(&mapped(word.as_bytes())));
                }
                Step::Remove(file) => {
                    put(b"remove");
                    put(&mapped(file.as_os_str().as_bytes()));
                }
            }
        }
        out
    }

    /// The command of step `at`, a program, in the environment `nvcc` gives
    /// it: the file found for it, run by its name.
    pub fn command(&self, at: usize) -> Option<Command> {
        let Step::Run { words, file } = &self.steps[at] else { return None };
        let mut command = Command::new(file.as_deref().unwrap_or(Path::new(&words[0])));
        command.arg0(&words[0]).args(&words[1..]).envs(self.env.iter().map(|(name, value)| (name, value)));
        if at == self.last {
            command.args(["-MD", "-MF"]).arg(&self.reads);
        }
        if let Some((_, file)) = self.pass_reads.iter().find(|(step, _)| *step == at) {
            command.args(["-MD", "-MF"]).arg(file);
        }
        Some(command)
    }

    /// Remove what `nvcc` would have removed, from step `from` on, and what
    /// the plan adds: what a compile that failed or was stopped leaves
    /// behind otherwise.
    pub fn tidy_from(&self, from: usize) {
        for step in self.steps.iter().skip(from) {
            if let Step::Remove(file) = step {
                let _ = fs::remove_file(file);
            }
        }
        let _ = fs::remove_file(&self.reads);
        for (_, file) in &self.pass_reads {
            let _ = fs::remove_file(file);
        }
    }

    /// The places the last step would look in for a file of `host_reads`
    /// (its identity's: what that step read in the trial) before it finds
    /// the file: the directory of the generated files that include them
    /// (the object's), then its `-iquote`, `-I`, `-isystem` and
    /// `-idirafter` directories in that order, up to the one that holds the
    /// file, each under the name the file has below that directory; and a
    /// precompiled header (`.gch`) beside each. The key reads none of these
    /// lookups (no pass makes them), so they must all find nothing: `Err`
    /// names one that does. A file below none of the directories (a system
    /// header) is looked for under each name it has below an `include`
    /// directory, in all of them.
    pub fn ahead(&self, host_reads: &[PathBuf]) -> Res<Vec<PathBuf>> {
        let Step::Run { words, .. } = &self.steps[self.last] else { return Ok(Vec::new()) };
        let object_dir = self.id_file.parent().map(Path::to_owned).unwrap_or_default();
        let mut dirs = vec![object_dir];
        for option in ["-iquote", "-I", "-isystem", "-idirafter"] {
            let mut words = words.iter();
            while let Some(word) = words.next() {
                if word == "-I-" {
                    bail!("its last step splits the include path (-I-)");
                }
                let dir = match word.strip_prefix(option) {
                    Some("") => words.next().cloned(),
                    Some(dir) if option == "-I" || dir.starts_with('/') => Some(dir.to_owned()),
                    _ => None,
                };
                dirs.extend(dir.map(PathBuf::from));
            }
        }
        let real: Vec<Option<PathBuf>> = dirs.iter().map(|dir| fs::canonicalize(dir).ok()).collect();
        let mut places = Vec::new();
        for read in host_reads {
            // Each name the file has below a directory searched, up to that
            // directory: whichever the file is included by, every place
            // before is looked at (the CUDA root given with `-I` as well as
            // its `include` directory: seen).
            let mut names: Vec<(PathBuf, usize)> = real
                .iter()
                .enumerate()
                .filter_map(|(at, dir)| Some((read.strip_prefix(dir.as_ref()?).ok()?.to_owned(), at)))
                .filter(|(name, _)| !name.as_os_str().is_empty())
                .collect();
            // Below none of them (a system header): each name it has below an
            // `include` directory, in all of them.
            if names.is_empty() {
                let parts: Vec<_> = read.components().collect();
                names = parts.iter().enumerate().filter(|(_, part)| part.as_os_str() == "include").map(|(at, _)| (parts[at + 1..].iter().collect(), dirs.len())).collect();
            }
            for (name, found) in names {
                let mut gch = name.clone().into_os_string();
                gch.push(".gch");
                for dir in &dirs[..found] {
                    // (CUDA's own directory given twice finds the file itself
                    // first: no other file stands ahead of it there.)
                    let place = dir.join(&name);
                    if fs::canonicalize(&place).ok().as_ref() != Some(read) {
                        places.push(place);
                    }
                    places.push(dir.join(&gch));
                }
                if let Some(dir) = dirs.get(found) {
                    places.push(dir.join(&gch));
                }
            }
        }
        places.sort();
        places.dedup();
        if let Some(there) = places.iter().find(|place| fs::symlink_metadata(place).is_ok()) {
            bail!("{} is where nvcc's last step looks for a file of CUDA's before CUDA's own", there.display());
        }
        Ok(places)
    }

    /// What the last step read, as it said (and the file it said it in
    /// removed), but for `nvcc`'s own intermediate files beside the object.
    /// `None`: it did not say.
    pub fn last_reads(&self) -> Option<Vec<PathBuf>> {
        self.said_read(&self.reads)
    }

    /// What the preprocessing steps read, all of them, as they said (the
    /// files they said it in removed). `None`: one did not say.
    pub fn pass_reads(&self) -> Option<Vec<PathBuf>> {
        let said: Vec<Option<Vec<PathBuf>>> = self.pass_reads.iter().map(|(_, file)| self.said_read(file)).collect();
        said.into_iter().try_fold(Vec::new(), |mut all, read| {
            all.extend(read?);
            Some(all)
        })
    }

    /// What a step said it read in `file` (removed), but for `nvcc`'s own
    /// intermediate files beside the object.
    fn said_read(&self, file: &Path) -> Option<Vec<PathBuf>> {
        let said = fs::read(file);
        let _ = fs::remove_file(file);
        let mut prefix = self.id_file.as_os_str().as_bytes().strip_suffix(b".module_id")?.to_vec();
        prefix.push(b'.');
        let files = depend_files(&said.ok()?)?;
        Some(files.into_iter().filter(|file| !file.as_os_str().as_bytes().starts_with(&prefix)).collect())
    }

    /// Run every step in turn, in `dir`, without a word to the terminal:
    /// for a trial. `Err` says what failed.
    fn run_quietly(&self, dir: &Path) -> Result<(), String> {
        self.give_id().map_err(|e| format!("cannot write {}: {e}", self.id_file.display()))?;
        for (at, step) in self.steps.iter().enumerate() {
            let Some(mut command) = self.command(at) else {
                if let Step::Remove(file) = step {
                    let _ = fs::remove_file(file);
                }
                continue;
            };
            let out = command.current_dir(dir).env("PWD", dir).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).output();
            let program = match step {
                Step::Run { words, .. } => words[0].as_str(),
                Step::Remove(_) => "",
            };
            match out {
                Ok(out) if out.status.success() => {}
                Ok(out) => {
                    self.tidy_from(at);
                    return Err(format!("{program} failed ({}): {}", out.status, String::from_utf8_lossy(&out.stderr)));
                }
                Err(e) => {
                    self.tidy_from(at);
                    return Err(format!("{program} could not run: {e}"));
                }
            }
        }
        Ok(())
    }
}


/// What `nvcc` says when a program it runs is killed by `signal` (its own
/// words, from its binary; CUDA 12.2).
pub fn died(program: &str, signal: i32) -> String {
    let why = match signal {
        4 => "(Illegal Instruction)",
        7 => "(Bus error)",
        8 => "(Floating point exception)",
        9 => "(Kill signal)",
        11 => "(Invalid memory reference)",
        _ => "",
    };
    format!("nvcc error   : '{program}' died due to signal {signal} {why}\n")
}

/// The module ID the cache gives a compile with the key `key` of the source
/// `source`: in the shape `cudafe++` gives its own (`_<hash>_<length>_<name>_
/// <hash>`), with the hashes from the key. The key differs where the
/// compile does (it names the source and the object, mapped), so two
/// translation units of one link never share an ID.
pub fn module_id(key: &str, source: &Path) -> String {
    let base = source.file_name().unwrap_or_default().as_bytes();
    let name: String = base.iter().map(|&b| if b.is_ascii_alphanumeric() { b as char } else { '_' }).collect();
    format!("_{}_{}_{}_{}", &key[..8], base.len(), name, &key[8..16])
}

/// Does `nvcc` (at `path`, run as `name`) take the module ID it is given
/// ([`Plan`]), and does it then make one object of the same CUDA source in
/// two places, under the path map's flags as the cache gives them
/// ([`map_flags`])? Tried in `dir`, with its default host compiler and the
/// relocatable device code Cactus asks for, as a GCC is tried
/// (`identity::relocates`). And what does its last step, the host compile
/// of what it generates, read that is not the trial's own?
pub(super) fn trial(path: &Path, name: &OsStr, dir: &Path) -> Trial {
    let failed = Trial { relocates: false, takes_id: false, host_reads: Vec::new() };
    let Ok(trial) = tempfile::Builder::new().prefix("cuda-trial-").tempdir_in(dir) else { return failed };
    // By its physical path, and with `PWD` naming it, as the GCC trial: the
    // host compiler records `$PWD` where it names the working directory
    // (tried: a `/work` link on qbd left the physical path unmapped).
    let Ok(trial_dir) = fs::canonicalize(trial.path()) else { return failed };
    // Without a routine or variable of external linkage to name the module
    // by: `cudafe++` would make the ID of where and when it compiles.
    let text = "namespace {\n__device__ int hidden;\n__device__ int twice(int v) { return 2 * v + hidden; }\n__global__ void k(int *p) { p[threadIdx.x] = twice(p[threadIdx.x]); }\nvoid launch(int *p) { k<<<1, 32>>>(p); }\n}\n";
    let id = module_id("cac7c0de5eed0f1d00000000", Path::new("trial.cu"));
    // Laid out as Cactus lays out a compile (the source copied into the
    // configuration's build directory, run from its scratch directory),
    // in trees and configurations of names of other lengths: the map has
    // the configuration directory inside the root to get right.
    let mut objects = Vec::new();
    let mut host_reads = None;
    for (place, config) in [("one/Cactus", "sim"), ("another/deeper/Cactus", "simulation")] {
        let root = trial_dir.join(place);
        let config = root.join("configs").join(config);
        let (build, scratch) = (config.join("build/Thorn"), config.join("scratch"));
        let source = build.join("trial.cu");
        let made = [&build, &scratch, &config.join("bindings/include")].iter().all(|dir| fs::create_dir_all(dir).is_ok());
        if !made || fs::write(&source, text).is_err() {
            return failed;
        }
        let object = build.join("trial.cu.o");
        let mut args: Vec<OsString> = ["-x", "cu", "-g", "-O2", "--relocatable-device-code=true", "--objdir-as-tempdir", "-c", "-o"].map(OsString::from).to_vec();
        args.push(object.clone().into_os_string());
        args.push(source.clone().into_os_string());
        args.push(OsString::from("-I"));
        args.push(config.join("bindings/include").into_os_string());
        args.extend(map_flags(&source, &PathMap::for_trial(&root, &config)));
        let mut command = Command::new(path);
        command.arg0(name).arg("-dryrun").args(&args).current_dir(&scratch).env("PWD", &scratch).stdin(Stdio::null()).stdout(Stdio::null());
        super::key::in_english(&mut command);
        let plan = match command.output() {
            Ok(out) => Plan::from_dry_run(DryRun::read(&String::from_utf8_lossy(&out.stderr)), id.clone()),
            Err(e) => Err(e.into()),
        };
        let ran = plan.map_err(|e| format!("{e:#}")).and_then(|plan| plan.run_quietly(&scratch).map(|()| plan.last_reads()));
        match ran {
            Ok(Some(reads)) => {
                host_reads.get_or_insert_with(|| reads.into_iter().filter(|read| !scratch.join(read).starts_with(&trial_dir)).collect::<Vec<_>>());
            }
            Ok(None) => {
                identity::trial_says(&format!("{}: the trial compile's last step did not say what it read", path.display()));
                return failed;
            }
            Err(why) => {
                identity::trial_says(&format!("{}: the trial compile: {why}", path.display()));
                return failed;
            }
        }
        match fs::read(&object) {
            Ok(object) => objects.push(object),
            Err(_) => return failed,
        }
    }
    let takes_id = objects.iter().all(|object| object.windows(id.len()).any(|window| window == id.as_bytes()));
    if !takes_id {
        identity::trial_says(&format!("{} did not take the module ID it was given", path.display()));
    } else if objects[0] != objects[1] {
        identity::trial_says(&format!("{} made two objects of the trial in two places", path.display()));
    }
    Trial { relocates: takes_id && objects[0] == objects[1], takes_id, host_reads: host_reads.unwrap_or_default() }
}

/// What [`trial`] found.
pub(super) struct Trial {
    pub relocates: bool,
    pub takes_id: bool,
    pub host_reads: Vec<PathBuf>,
}

/// The flags that have an `nvcc` compile of `source` honor the path map
/// `map`: the host compiler's prefix map (through `-Xcompiler`, shortest
/// directory first, as the host compiler takes the last map that matches),
/// and the name `cudafe++` is given for the source, of which the module ID
/// is a hash.
pub fn map_flags(source: &Path, map: &PathMap) -> Vec<OsString> {
    let mut flags = Vec::new();
    for flag in map.flags() {
        flags.push(OsString::from("-Xcompiler"));
        flags.push(flag);
    }
    flags.push(OsString::from("-Xcudafe"));
    let mut name = OsString::from("--orig_src_path_name=");
    name.push(OsStr::from_bytes(&map.apply(source.as_os_str().as_bytes())));
    flags.push(name);
    flags
}

/// The host compiler the preprocessor passes of `run` would run (the one
/// `--compiler-bindir` names, or the one `nvcc` finds along the `PATH` it
/// gives), identified as any compiler is, with what its identity watches.
pub(super) fn host(run: &DryRun, cc_dir: &Path) -> Res<(Compiler, identity::Watch)> {
    let mut passes = run.preprocessor_passes();
    let first = passes.next().context("nvcc would run no preprocessor")?;
    if passes.any(|pass| pass[0] != first[0]) {
        bail!("nvcc would run more than one host compiler");
    }
    // Along the `PATH` it gives, with the places passed over watched: a
    // `g++` that turns up ahead of this one is another host compiler.
    let program = OsStr::new(&first[0]);
    let search = run.get("PATH").context("nvcc does not say where it finds the host compiler")?;
    let (found, files, absent) = identity::search_watching(program, OsStr::new(search))?;
    let found = found.with_context(|| format!("nvcc would run {}, which is not on its PATH", program.to_string_lossy()))?;
    let (host, watch) = identity::identify_watching(cc_dir, found.as_os_str())?;
    let watch = watch.with_places(files, absent);
    if !matches!(host.family, Family::Gcc | Family::Clang) {
        bail!("nvcc's host compiler {} is not a C++ compiler cactup knows", found.display());
    }
    Ok((host, watch))
}

/// What a CUDA compile's key was made from, kept for the check after it.
#[derive(Debug)]
pub struct Cuda {
    pub host: Compiler,
    /// The preprocessor passes as run (program first), and the environment
    /// `nvcc` gives them.
    passes: Vec<Vec<String>>,
    env: Vec<(String, String)>,
    /// The passes say which macros the text expands or tests (`-dU`): a
    /// GCC host compiler, without `-g3`; the same in the check after the
    /// compile as in the key.
    macro_use: bool,
    /// Where they do and say nothing of the text, the definitions of the
    /// others are not keyed (decisions 26, 30).
    used: Option<Used>,
    /// What the key's passes said they read (`-MD`, decision 28).
    keyed_reads: BTreeSet<PathBuf>,
}

/// The key's text and files for a CUDA compile: what its preprocessor
/// passes print and read.
pub struct Read {
    pub text: String,
    pub text_bytes: u64,
    pub files: String,
    /// The files whole (`key::read_files`).
    pub whole: String,
    pub count: usize,
    pub seen: String,
    pub cuda: Cuda,
}

/// Key a CUDA compile whose dry run is `run`: run its preprocessor passes
/// (the host compiler's `-E`, for the host and for each device
/// architecture), each as `nvcc` would, in the environment it gives them,
/// but printing (no `-o`), saying where it takes flags from (`-v`), and
/// with the path map's flags; and digest what they print and the files
/// they read, as for C and C++.
pub fn key(run: &DryRun, host: Compiler, compile: &Compile, map: Option<&PathMap>, narrow: bool) -> Res<Read> {
    let passes: Vec<Vec<String>> = run.preprocessor_passes().cloned().collect();
    let used = (narrow && host.family == Family::Gcc && !compile.macros_in_debug).then(|| Used::new(Default::default(), Default::default(), Default::default(), compile));
    let macro_use = used.is_some();
    let mut cuda = Cuda { host, passes, env: run.env.clone(), macro_use, used, keyed_reads: BTreeSet::new() };
    let Passes { text, bytes: text_bytes, named, used: names, with_code, diagnosed, reads } = run_passes(&cuda, map, true)?;
    cuda.keyed_reads = reads.into_iter().collect();
    // Narrowed but in the files the passes' diagnostics name (as for C and
    // C++); not at all where one names no file; a macro the passes are given
    // on their own command lines
    // (`-DCUDA_DOUBLE_MATH_FUNCTIONS`) counts as given (a header's
    // definition of it draws a warning).
    cuda.used = match (cuda.used.take(), diagnosed) {
        (Some(_), Some(diagnosed)) => {
            let mut used = Used::new(names, with_code, diagnosed, compile);
            for pass in &cuda.passes {
                used.also_given(pass);
            }
            Some(used)
        }
        _ => None,
    };
    let mut named = named;
    // The source itself, whatever the markers call it.
    let source = compile.source.as_os_str().as_bytes();
    named.insert((map.map_or_else(|| source.to_vec(), |map| map.apply(source)), compile.source.clone()), true);
    let (files, whole, seen) = read_files(&named, map, cuda.used.as_ref())?;
    Ok(Read { text, text_bytes, files, whole, count: named.len(), seen, cuda })
}

/// What [`run_passes`] found: the passes' text digested and its length, the
/// files they named, the macros they say the text uses (`-dU`), the files
/// the text has code of, whether they said anything of the text, and what
/// they said they read.
struct Passes {
    text: String,
    bytes: u64,
    named: Named,
    used: BTreeSet<Vec<u8>>,
    with_code: BTreeSet<PathBuf>,
    /// The files the passes' diagnostics name (`key::diagnosed_files`).
    diagnosed: Option<BTreeSet<PathBuf>>,
    /// What the passes said they read (`-MD`), where asked.
    reads: Vec<PathBuf>,
}

/// Run the passes of `cuda` again, and digest them: the text of each pass in
/// turn, and every file any of them names; with `list`, each also says what
/// it read (`-MD`).
fn run_passes(cuda: &Cuda, map: Option<&PathMap>, list: bool) -> Res<Passes> {
    let mut text = Hasher::new("cuda passes");
    let (mut bytes, mut named, mut used, mut diagnosed) = (0u64, Named::new(), BTreeSet::new(), Some(BTreeSet::new()));
    let mut with_code = BTreeSet::new();
    let mut reads = Vec::new();
    for pass in &cuda.passes {
        let mut args: Vec<&String> = Vec::new();
        let mut words = pass.iter().skip(1);
        while let Some(word) = words.next() {
            match word.as_str() {
                // What the pass prints is what is keyed, not a file.
                "-o" => {
                    words.next();
                }
                _ => args.push(word),
            }
        }
        let mut command = Command::new(&cuda.host.path);
        command.args(args).arg("-v");
        if cuda.macro_use {
            command.arg("-dU");
        }
        // What the pass reads, as the compile's own passes will say it
        // (decision 28).
        let listed = match list {
            true => Some(tempfile::Builder::new().prefix(".cactup-").suffix(".d").tempfile()?.into_temp_path()),
            false => None,
        };
        if let Some(listed) = &listed {
            command.args(["-MD", "-MF"]).arg(listed);
        }
        if let Some(map) = map {
            command.args(map.flags());
        }
        for (name, value) in &cuda.env {
            command.env(name, value);
        }
        super::key::in_english(&mut command);
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("Failed to run {} as nvcc's preprocessor", cuda.host.path.display()))?;
        PREPROCESSOR.store(child.id() as i32, Ordering::SeqCst);
        let said = child.stderr.take().map(|mut stderr| {
            std::thread::spawn(move || {
                let mut said = Vec::new();
                let _ = std::io::Read::read_to_end(&mut stderr, &mut said);
                said
            })
        });
        let read = digest_output(&mut child, map);
        let status = child.wait();
        PREPROCESSOR.store(0, Ordering::SeqCst);
        let said = said.and_then(|said| said.join().ok()).unwrap_or_default();
        let super::key::Output { text: pass_text, bytes: pass_bytes, named: pass_named, used: pass_used, with_code: pass_code, diagnostic_pragma } = read?;
        used.extend(pass_used);
        with_code.extend(pass_code);
        if !status.context("Failed to wait for nvcc's preprocessor")?.success() {
            bail!("nvcc's preprocessor failed");
        }
        flags_from_elsewhere(cuda.host.family, cuda.host.specs.as_deref(), &said).map_err(anyhow::Error::msg)?;
        match (&mut diagnosed, diagnosed_files(&said).filter(|_| said_as_compiled(&said, diagnostic_pragma))) {
            (Some(all), Some(these)) => all.extend(these),
            _ => diagnosed = None,
        }
        if let Some(listed) = listed {
            reads.extend(fs::read(&listed).ok().as_deref().and_then(depend_files).context("nvcc's preprocessor did not say what it read")?);
        }
        text.feed(pass_text.as_bytes());
        // And what it said of the text, which no output shows (as for C and
        // C++: `key::said_of_the_text`).
        text.feed(&said_of_the_text(&said, map));
        bytes += pass_bytes;
        for (name, entered) in pass_named {
            *named.entry(name).or_default() |= entered;
        }
    }
    Ok(Passes { text: text.hex(), bytes, named, used, with_code, diagnosed, reads })
}

/// Does the key of a CUDA compile still hold after it: the same passes print
/// the same text, from the same files, which look as they did?
/// And the compile's last step (`plan`'s) read nothing the key does not
/// cover: files the passes read, and those of `host_reads` (its identity's);
/// and its preprocessing steps read just what the key's passes did
/// (decision 28).
pub fn still_holds(cuda: &Cuda, plan: &Plan, host_reads: &[PathBuf], compile: &Compile, map: Option<&PathMap>, parts: &Parts, seen: &str) -> bool {
    let (text, files) = (parts.text.as_str(), parts.files.as_str());
    let (last_read, pass_read) = match plan.ran() {
        true => match (plan.last_reads(), plan.pass_reads()) {
            (Some(last), Some(passes)) => (last, Some(passes)),
            _ => return false,
        },
        false => (Vec::new(), None),
    };
    if pass_read.is_some_and(|pass_read| !reads_match(&pass_read, &cuda.keyed_reads)) {
        return false;
    }
    let Ok(Passes { text: now_text, mut named, .. }) = run_passes(cuda, map, false) else { return false };
    let passes: std::collections::HashSet<&Path> = named.keys().map(|(_, real)| real.as_path()).collect();
    let covered = |read: &PathBuf| {
        passes.contains(read.as_path()) || fs::canonicalize(read).is_ok_and(|real| host_reads.contains(&real) || passes.contains(real.as_path()))
    };
    if !last_read.iter().all(covered) {
        return false;
    }
    let source = compile.source.as_os_str().as_bytes();
    named.insert((map.map_or_else(|| source.to_vec(), |map| map.apply(source)), compile.source.clone()), true);
    now_text == text && read_files(&named, map, cuda.used.as_ref()).is_ok_and(|(now_files, _, now_seen)| now_files == files && now_seen == seen)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What CUDA 13.2's `nvcc -dryrun` said on saturn for a `-c` of
    /// `/dev/null` (shortened: the flags that matter here kept).
    const SATURN: &str = r#"#$ _NVVM_BRANCH_=nvvm
#$ _SPACE_=
#$ _HERE_=/packages/cuda/13.2.1/bin
#$ TOP=/packages/cuda/13.2.1/bin/..
#$ CICC_PATH=/packages/cuda/13.2.1/bin/../nvvm/bin
#$ NVVMIR_LIBRARY_DIR=/packages/cuda/13.2.1/bin/../nvvm/libdevice
#$ LD_LIBRARY_PATH=/packages/cuda/13.2.1/bin/../lib:
#$ PATH=/packages/cuda/13.2.1/bin/../nvvm/bin:/packages/cuda/13.2.1/bin:/usr/bin
#$ INCLUDES="-I/packages/cuda/13.2.1/bin/../targets/x86_64-linux/include"
#$ "/packages/gcc/13.2.0/bin"/g++ -std=c++17 -D__CUDA_ARCH_LIST__=800 -E -x c++ -D__CUDACC__ "-I/packages/cuda/13.2.1/bin/../targets/x86_64-linux/include" "/dev/null" -o "/dev/null.cpp4.ii"
#$ cudafe++ --c++17 --orig_src_file_name "/dev/null" --orig_src_path_name "/dev/null" --gen_c_file_name "/dev/null.cudafe1.cpp" --module_id_file_name "/dev/null.module_id" "/dev/null.cpp4.ii"
#$ "/packages/gcc/13.2.0/bin"/g++ -std=c++17 -D__CUDA_ARCH__=800 -D__CUDA_ARCH_LIST__=800 -E -x c++ -DCUDA_DOUBLE_MATH_FUNCTIONS "/dev/null" -o "/dev/null.cpp1.ii"
#$ "$CICC_PATH/cicc" --c++17 -arch compute_80 -m64 "/dev/null.cpp1.ii" -o "/dev/null.ptx"
#$ ptxas -arch=sm_80 -m64 --compile-only  "/dev/null.ptx"  -o "/dev/null.sm_80.cubin"
#$ fatbinary --create="/dev/null.fatbin" -64 "--image3=kind=elf,sm=80,file=/dev/null.sm_80.cubin"
#$ "/packages/gcc/13.2.0/bin"/g++ -std=c++17 -D__CUDA_ARCH__=800 -c -x c++ "/dev/null.cudafe1.cpp" -o "/dev/null"
#$ rm /dev/null.sm_80.cubin
"#;

    #[test]
    fn reads_what_nvcc_would_run() {
        let run = DryRun::read(SATURN);
        assert_eq!(run.get("CICC_PATH"), Some("/packages/cuda/13.2.1/bin/../nvvm/bin"));
        assert_eq!(run.get("LD_LIBRARY_PATH"), Some("/packages/cuda/13.2.1/bin/../lib:"));
        assert_eq!(run.get("_SPACE_"), Some(""));
        // The host preprocessor runs twice: the host pass, then the device's.
        let passes: Vec<_> = run.preprocessor_passes().collect();
        assert_eq!(passes.len(), 2);
        assert_eq!(passes[0][0], "/packages/gcc/13.2.0/bin/g++");
        assert!(passes[1].iter().any(|word| word == "-D__CUDA_ARCH__=800"));
        assert!(passes[0].iter().any(|word| word == "-I/packages/cuda/13.2.1/bin/../targets/x86_64-linux/include"));
        // `cicc` by the path `nvcc` gives it, `$CICC_PATH` taken from above.
        assert_eq!(run.programs(), ["/packages/cuda/13.2.1/bin/../nvvm/bin/cicc", "cudafe++", "fatbinary", "ptxas"]);
    }

    /// What qbd's `nvcc` 12.2 would run for a compile with
    /// `--objdir-as-tempdir` (shortened).
    const QBD: &str = r#"#$ _NVVM_BRANCH_=nvvm
#$ PATH=/cuda/bin/../nvvm/bin:/cuda/bin:/usr/bin
#$ g++ -D__CUDA_ARCH__=800 -E -x c++ "/w/t.cxx" -o "/w/t.o.cpp1.ii"
#$ cudafe++ --c++17 --orig_src_path_name "/w/t.cxx" --gen_c_file_name "/w/t.o.cudafe1.cpp" --gen_module_id_file --module_id_file_name "/w/t.o.module_id" "/w/t.o.cpp4.ii"
#$ cicc --c++17 --module_id_file_name "/w/t.o.module_id" "/w/t.o.cpp1.ii" -o "/w/t.o.ptx"
#$ g++ -c -x c++ "/w/t.o.cudafe1.cpp" -o "/w/t.o"
#$ rm /w/t.o.cudafe1.cpp
#$ rm /w/t.o.module_id
"#;

    #[test]
    fn repeats_what_nvcc_would_run_with_the_module_id_given() {
        let plan = Plan::from_dry_run(DryRun::read(QBD), "_x".to_owned()).unwrap();
        assert_eq!(plan.id_file, Path::new("/w/t.o.module_id"));
        // The last host compile says what it read.
        assert_eq!(plan.last, 3);
        assert_eq!(plan.reads, Path::new(&format!("/w/.cactup-{}-last.d", std::process::id())));
        assert_eq!(plan.env, [("_NVVM_BRANCH_".to_owned(), "nvvm".to_owned()), ("PATH".to_owned(), "/cuda/bin/../nvvm/bin:/cuda/bin:/usr/bin".to_owned())]);
        let words: Vec<String> = plan
            .steps()
            .iter()
            .map(|step| match step {
                Step::Run { words, .. } => words.join(" "),
                Step::Remove(file) => format!("remove {}", file.display()),
            })
            .collect();
        assert_eq!(
            words,
            [
                "g++ -D__CUDA_ARCH__=800 -E -x c++ /w/t.cxx -o /w/t.o.cpp1.ii",
                // Told to read the ID, not to make one: nothing else changes.
                "cudafe++ --c++17 --orig_src_path_name /w/t.cxx --gen_c_file_name /w/t.o.cudafe1.cpp --module_id_file_name /w/t.o.module_id /w/t.o.cpp4.ii",
                "cicc --c++17 --module_id_file_name /w/t.o.module_id /w/t.o.cpp1.ii -o /w/t.o.ptx",
                "g++ -c -x c++ /w/t.o.cudafe1.cpp -o /w/t.o",
                "remove /w/t.o.cudafe1.cpp",
                "remove /w/t.o.module_id",
            ]
        );
        // What it cannot repeat as it is: no module ID made in a file, or
        // two, or an `rm` of anything but one file.
        for (from, to) in [
            (" --gen_module_id_file", ""),
            (" --module_id_file_name \"/w/t.o.module_id\" \"/w/t.o.cpp4.ii\"", " \"/w/t.o.cpp4.ii\""),
            ("#$ rm /w/t.o.module_id", "#$ rm -f /w/t.o.module_id"),
        ] {
            assert!(QBD.contains(from), "{from}");
            assert!(Plan::from_dry_run(DryRun::read(&QBD.replace(from, to)), "_x".to_owned()).is_err(), "{from}");
        }
        let twice = QBD.replace("#$ cicc", "#$ cudafe++ --gen_module_id_file --module_id_file_name \"/w/u\" x\n#$ cicc");
        assert!(Plan::from_dry_run(DryRun::read(&twice), "_x".to_owned()).is_err());
    }

    #[test]
    fn keeps_what_nvcc_says_itself_and_refuses_what_it_cannot_repeat() {
        let warned = format!("nvcc warning : incompatible redefinition for option 'compiler-bindir', the last value of this option was used\n{QBD}");
        let plan = Plan::from_dry_run(DryRun::read(&warned), "_x".to_owned()).unwrap();
        assert_eq!(plan.said(), b"nvcc warning : incompatible redefinition for option 'compiler-bindir', the last value of this option was used\n");
        let shelly = QBD.replace("#$ cicc --c++17", "#$ cicc --c++17 $(id -u)");
        assert!(Plan::from_dry_run(DryRun::read(&shelly), "_x".to_owned()).is_err());
        // No host compile of the generated code: nothing says what it read.
        let without = QBD.replace("#$ g++ -c -x c++", "#$ g++ -x c++");
        assert!(Plan::from_dry_run(DryRun::read(&without), "_x".to_owned()).is_err());
    }

    #[test]
    fn what_the_last_step_finds_by_searching_must_be_cudas_own() {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let (cuda, user, objects) = (root.join("cuda/include"), root.join("thorn/src"), root.join("build"));
        for dir in [cuda.join("crt"), user.clone(), objects.clone()] {
            fs::create_dir_all(dir).unwrap();
        }
        let header = cuda.join("crt/host_runtime.h");
        fs::write(&header, "").unwrap();
        let o = objects.join("t.o");
        let said = format!(
            "#$ cudafe++ --gen_module_id_file --module_id_file_name \"{o}.module_id\" \"{o}.cpp4.ii\"\n#$ g++ -c -x c++ -I{user} \"-I{cuda}\" \"{o}.cudafe1.cpp\" -o \"{o}\"\n",
            o = o.display(),
            user = user.display(),
            cuda = cuda.display()
        );
        let plan = Plan::from_dry_run(DryRun::read(&said), "_x".to_owned()).unwrap();
        let places = plan.ahead(std::slice::from_ref(&header)).unwrap();
        // The generated files' own directory, then the thorn's, then a
        // precompiled header beside CUDA's own.
        for place in ["build/crt/host_runtime.h", "build/crt/host_runtime.h.gch", "thorn/src/crt/host_runtime.h", "thorn/src/crt/host_runtime.h.gch", "cuda/include/crt/host_runtime.h.gch"] {
            assert!(places.contains(&root.join(place)), "{place}");
        }
        assert!(!places.contains(&header));
        // A thorn's header of that name, ahead of CUDA's: not cached.
        fs::create_dir_all(user.join("crt")).unwrap();
        fs::write(user.join("crt/host_runtime.h"), "").unwrap();
        assert!(plan.ahead(std::slice::from_ref(&header)).is_err());
        // Also with CUDA's root searched first, which holds the header too
        // (under a longer name).
        let rooted = said.replace("#$ g++ -c -x c++ ", &format!("#$ g++ -c -x c++ -I{} ", root.join("cuda").display()));
        let plan = Plan::from_dry_run(DryRun::read(&rooted), "_x".to_owned()).unwrap();
        assert!(plan.ahead(std::slice::from_ref(&header)).is_err());
        fs::remove_file(user.join("crt/host_runtime.h")).unwrap();
        let places = plan.ahead(std::slice::from_ref(&header)).unwrap();
        assert!(places.contains(&user.join("crt/host_runtime.h")));
    }

    #[test]
    fn reads_what_the_last_step_says_it_read() {
        let said = b"/w/t.o: /w/t.o.cudafe1.cpp \\\n /cuda/include/crt/host_runtime.h /a\\ b.h \\\n /c$$d.h\n";
        assert_eq!(
            depend_files(said).unwrap(),
            ["/w/t.o.cudafe1.cpp", "/cuda/include/crt/host_runtime.h", "/a b.h", "/c$d.h"].map(PathBuf::from)
        );
        assert!(depend_files(b"no target here").is_none());
        assert!(depend_files(b"a: b\nc: d\n").is_none());
    }

    #[test]
    fn says_what_nvcc_says_of_a_program_killed() {
        assert_eq!(died("cicc", 9), "nvcc error   : 'cicc' died due to signal 9 (Kill signal)\n");
        assert_eq!(died("cicc", 1), "nvcc error   : 'cicc' died due to signal 1 \n");
    }

    #[test]
    fn gives_a_module_id_in_the_shape_of_cudafes() {
        let key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(module_id(key, Path::new("/w/build/T/init.cxx")), "_01234567_8_init_cxx_89abcdef");
        assert_eq!(module_id(key, Path::new("a-b.cu")), "_01234567_6_a_b_cu_89abcdef");
    }

    #[test]
    fn maps_the_configuration_inside_the_root() {
        // GCC takes the last map that matches: the root's first, then the
        // configuration's (seen on qbd, the other way round: every config
        // dir left as `/cactup-root//configs/<name>`).
        let map = PathMap::for_trial(Path::new("/w/Cactus"), Path::new("/w/Cactus/configs/sim"));
        let flags = map_flags(Path::new("/w/Cactus/configs/sim/build/T/a.cu"), &map);
        let flags: Vec<&str> = flags.iter().map(|flag| flag.to_str().unwrap()).collect();
        assert_eq!(
            flags,
            [
                "-Xcompiler",
                "-ffile-prefix-map=/w/Cactus/=/cactup-root/",
                "-Xcompiler",
                "-ffile-prefix-map=/w/Cactus/configs/sim/=/cactup-root/configs/@config/",
                "-Xcudafe",
                "--orig_src_path_name=/cactup-root/configs/@config/build/T/a.cu",
            ]
        );
    }

    #[test]
    fn splits_words_as_the_shell_does() {
        let get = |name: &str| (name == "X").then(|| "/x".to_owned());
        assert_eq!(shell_words(r#""/a b"/c -d 'e f' g\ h $X/y ${X}z $NO "$NO""#, get).unwrap(), ["/a b/c", "-d", "e f", "g h", "/x/y", "/xz", ""]);
        assert_eq!(shell_words("  ", get).unwrap(), Vec::<String>::new());
        // A backslash in double quotes escapes only what the shell lets it.
        assert_eq!(shell_words(r#""a\b" "c\"d" "e\$X""#, get).unwrap(), [r"a\b", "c\"d", "e$X"]);
        assert_eq!(shell_words("a $ b", get).unwrap(), ["a", "$", "b"]);
        let blank = |name: &str| (name == "B").then(|| "two words".to_owned());
        assert!(shell_words("a $B", blank).is_err());
        assert_eq!(shell_words(r#"a "$B""#, blank).unwrap(), ["a", "two words"]);
        for more in ["a `b`", "a $(b)", "a; b", "a | b", "a > b", "a*", "~/a", "${X:-y}", "$@", "'a"] {
            assert!(shell_words(more, get).is_err(), "{more}");
        }
    }
}
