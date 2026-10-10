//! Which compiler a command line runs: the part of a cache key that says
//! "the same program would run", by content and not by name.
//!
//! A compiler is identified by the bytes of what actually does the work —
//! the driver the command names, for GCC the back ends and assembler it
//! runs, and the shared libraries each program loads (a distribution's
//! Clang is a small driver in front of `libclang-cpp.so`) — plus what it
//! says about itself (version, target, built-in specs) and the name it was
//! run by (`clang` and `clang++` are one file). Its path
//! is not part of it, so two installations of the same compiler agree; its
//! modification time is not either, so one compiler copied twice does.
//!
//! Only a compiler cactup can identify *as itself* is cached. A wrapper
//! (`mpicc`, a Cray `cc`, a site script) adds flags and picks a compiler by
//! rules of its own; it passes `--version` through to the compiler, and so
//! looks like one. The driver's own bytes have to say what it is.
//!
//! For the same reason a compiler that takes flags from a file of its own
//! is not cached: a GCC with a `specs` file on disk, a Clang that reads a
//! configuration file. What such a file adds never passes the reader of
//! the command line (`compile`), so nothing the cache declines there — a
//! flag that records the command line, one that puts unmapped paths into
//! the object — would be declined. The one exception is a specs file that
//! changes only how GCC links (`specs`).
//!
//! What this cannot see: files a compiler reads by rules of its own that
//! are named nowhere here — a plugin directory, lists in Clang's resource
//! directory (which is why sanitizers are not cached), anything a later
//! compiler version adds. The list above is what is known.
//!
//! Hashing a compiler takes a moment (GCC's `cc1plus` is tens of megabytes)
//! and every compile of a build asks, so the answer — also the answer "not
//! one the cache works with" — is kept for the length of one build
//! attempt, in `<attempt>/cc/compilers/`, and reused as long as every file
//! it was computed from still looks the same (size, change time, inode).

use super::hash::{bytes_digest, file_digest, Hasher};
use crate::Res;
use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The compiler families the cache has an argument reader for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Family {
    Gcc,
    Clang,
    /// GCC's Fortran driver (§18.10).
    Gfortran,
}

/// An identified compiler.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Compiler {
    /// The file that runs when the command line names this compiler, as an
    /// absolute path. The wrapper starts *this*, under the name the recipe
    /// used: what was identified and what runs must be one file.
    pub path: PathBuf,
    pub family: Family,
    /// Does this compiler, given the path map of `key::PathMap`, make one
    /// object of the same sources in two different places? Found by trying
    /// ([`relocates`]): the map stands on it.
    pub relocates: bool,
    /// Does this compiler make the same object of a source with non-ASCII
    /// bytes in the session's locale as in the C locale? Found by trying
    /// ([`locale_neutral`]): if so, the locale is not keyed (§18.8).
    pub locale_neutral: bool,
    /// The digest that stands for this compiler in a key.
    pub id: String,
    /// The `specs` file a GCC reads, by its physical path, when it was
    /// accepted for changing only how GCC links (`specs::link_only`).
    /// Every compile must say it read this file and no other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub specs: Option<PathBuf>,
}

/// One file an identity was computed from, as it looked then.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct Seen {
    path: PathBuf,
    size: u64,
    ctime: i64,
    ctime_nsec: i64,
    ino: u64,
    dev: u64,
}

impl Seen {
    pub(super) fn of(path: &Path) -> Res<Self> {
        let meta = fs::metadata(path).with_context(|| format!("Failed to look at {}", path.display()))?;
        Ok(Self {
            path: path.to_owned(),
            size: meta.len(),
            // Change time, not modification time: `touch -r` and `cp -p`
            // can set the latter back.
            ctime: meta.ctime(),
            ctime_nsec: meta.ctime_nsec(),
            ino: meta.ino(),
            dev: meta.dev(),
        })
    }
}

/// What is kept between the compiles of one build attempt.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct Remembered {
    /// The compiler, or …
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compiler: Option<Compiler>,
    /// … why it is not one the cache works with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rejected: Option<String>,
    /// The environment that decides which helper programs the driver runs,
    /// as it was.
    env: String,
    files: Vec<Seen>,
    /// The places searched before each file was found (by the driver for
    /// its programs, along `PATH`, by the loader for libraries) that held
    /// nothing: one that holds something now may be found first. A relative
    /// one is looked at from the compile's own working directory.
    #[serde(default)]
    absent: Vec<PathBuf>,
}

/// The variables that decide which back ends and libraries a driver picks
/// up, and whether Clang reads its configuration files. A change in one of
/// them may mean another compiler under the same driver.
const HELPER_ENV: &[&str] =
    &["PATH", "COMPILER_PATH", "GCC_EXEC_PREFIX", "LD_LIBRARY_PATH", "LD_PRELOAD", "CLANG_NO_DEFAULT_CONFIG"];

fn helper_env() -> String {
    let mut hasher = Hasher::new("compiler-env");
    for name in HELPER_ENV {
        hasher.feed(name.as_bytes());
        hasher.feed(std::env::var_os(name).unwrap_or_default().as_bytes());
    }
    // And the locale, which the locale trial's answer is for.
    let mut locale: Vec<_> = std::env::vars_os()
        .filter(|(name, _)| name.as_bytes().starts_with(b"LC_") || name == "LANG" || name == "LANGUAGE")
        .collect();
    locale.sort();
    for (name, value) in locale {
        hasher.feed(name.as_bytes());
        hasher.feed(value.as_bytes());
    }
    hasher.hex()
}

/// The file `execvp` would run for `program` from this working directory,
/// as an absolute path: the name itself if it has a `/`, else the first
/// executable regular file of that name on `PATH` — relative entries and
/// empty ones (the working directory) included, as `execvp` includes them.
pub fn find_program(program: &OsStr) -> Res<PathBuf> {
    search_path(program, &mut Watched { files: &mut Vec::new(), absent: &mut Vec::new() })
}

/// [`find_program`], with the places passed over on the way watched.
fn search_path(program: &OsStr, watched: &mut Watched) -> Res<PathBuf> {
    let absolute = |path: &Path| std::path::absolute(path).with_context(|| format!("Failed to resolve {}", path.display()));
    if program.as_bytes().contains(&b'/') {
        return absolute(Path::new(program));
    }
    let executable = |path: &PathBuf| path.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    let path = std::env::var_os("PATH").context("PATH is not set")?;
    let dirs = path.as_bytes().split(|b| *b == b':').map(|dir| if dir.is_empty() { Path::new(".") } else { Path::new(OsStr::from_bytes(dir)) });
    for place in dirs.map(|dir| dir.join(program)) {
        if executable(&place) {
            return absolute(&place);
        }
        watched.passed(&place);
    }
    bail!("{} is not on PATH", program.to_string_lossy())
}

/// What an identity is computed from: the files it read, and the places
/// passed over before each file was found.
pub(super) struct Watched<'a> {
    pub(super) files: &'a mut Vec<Seen>,
    pub(super) absent: &'a mut Vec<PathBuf>,
}

impl Watched<'_> {
    /// A place searched and passed over: one that holds nothing must go on
    /// holding nothing, and anything there (a directory, a file the search
    /// rejected) must stay as it is.
    fn passed(&mut self, place: &Path) {
        match Seen::of(place) {
            Ok(seen) => self.files.push(seen),
            Err(_) => self.absent.push(place.to_owned()),
        }
    }
}

/// Identify the compiler `program` names, reusing what an earlier compile of
/// this build attempt found if nothing has changed since. `Err` says why
/// this is not a compiler the cache works with.
pub fn identify(cc_dir: &Path, program: &OsStr) -> Res<Compiler> {
    let path = find_program(program)?;
    // The name it is run by is part of what it is.
    let name = Path::new(program).file_name().unwrap_or(program);
    let memo_dir = cc_dir.join("compilers");
    let mut memo_name = Hasher::new("compiler-memo");
    memo_name.feed(path.as_os_str().as_bytes());
    memo_name.feed(name.as_bytes());
    let memo = memo_dir.join(format!("{}.toml", memo_name.hex()));
    let env = helper_env();
    if let Some(remembered) = fs::read_to_string(&memo).ok().and_then(|text| toml::from_str::<Remembered>(&text).ok())
        && remembered.env == env
        && remembered.files.iter().all(|seen| Seen::of(&seen.path).is_ok_and(|now| now == *seen))
        && remembered.absent.iter().all(|place| fs::metadata(place).is_err())
    {
        match (remembered.compiler, remembered.rejected) {
            (Some(compiler), _) => return Ok(compiler),
            (None, Some(why)) => bail!("{why}"),
            (None, None) => {}
        }
    }

    let mut files = Vec::new();
    let mut absent = Vec::new();
    let examined = fs::create_dir_all(&memo_dir)
        .with_context(|| format!("Failed to create {}", memo_dir.display()))
        .and_then(|()| examine(&path, name, &memo_dir, &mut Watched { files: &mut files, absent: &mut absent }));
    // Best-effort: without it the next compile just looks again. Written
    // whole and moved into place, since every compile of the build reads it.
    let remembered = Remembered {
        compiler: examined.as_ref().ok().cloned(),
        rejected: examined.as_ref().err().map(|e| format!("{e:#}")),
        env,
        files,
        absent,
    };
    if let Ok(text) = toml::to_string(&remembered)
        && let Ok(mut temp) = tempfile::NamedTempFile::new_in(&memo_dir)
        && temp.write_all(text.as_bytes()).is_ok()
    {
        let _ = temp.persist(&memo);
    }
    examined
}

/// Run `program` with `args` and return its standard output, if it succeeds.
fn ask(program: &Path, args: &[&str]) -> Res<String> {
    // In English: what a driver says of itself goes into its identity, and
    // must not depend on the language of the session that asked first.
    let mut command = Command::new(program);
    super::key::in_english(&mut command);
    let out = command
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .with_context(|| format!("Failed to run {}", program.display()))?;
    if !out.status.success() {
        bail!("{} {} failed ({})", program.display(), args.join(" "), out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The shared libraries the dynamic loader would load for `program`, as it
/// resolves them in this environment. This is what `ldd` does: with
/// `LD_TRACE_LOADED_OBJECTS` set, the loader lists them and exits without
/// running the program. A statically linked program has no loader to read
/// the variable and simply runs, which is why it is run as `--version`.
///
/// Where the loader looked first is watched too: it says so itself
/// (`LD_DEBUG=libs`, glibc's). Each place it tried and passed over, and each
/// directory on a search path that is not there (it is not tried again for
/// the next library, but would be once it is there); the cache it read. A
/// library found by a relative name (an empty `LD_LIBRARY_PATH` entry is the
/// working directory) is one each compile's directory may have another of:
/// that compiler is not one the cache works with.
fn loaded_libraries(program: &Path, watched: &mut Watched) -> Res<Vec<PathBuf>> {
    let out = Command::new(program)
        .arg("--version")
        .env("LD_TRACE_LOADED_OBJECTS", "1")
        .env("LD_DEBUG", "libs")
        .stdin(Stdio::null())
        .output();
    let Ok(out) = out else { return Ok(Vec::new()) };
    // `\tlibisl.so.23 => /lib/x86_64-linux-gnu/libisl.so.23 (0x00007f…)`,
    // or a library preloaded by its path: `\t/opt/lib/libx.so (0x…)`.
    let listed = String::from_utf8_lossy(&out.stdout);
    let library = |line: &str| {
        let line = line.trim();
        let at = line.split_once(" => ").map_or(line, |(_, at)| at);
        Some(PathBuf::from(at.rsplit_once(" (")?.0))
    };
    let libraries = listed.lines().filter_map(library).filter(|path| path.is_absolute()).collect();
    // `  1234:\t find library=libz.so.1 [0]; searching`, then its search
    // paths and tries, the last try the one found.
    let said = String::from_utf8_lossy(&out.stderr);
    let mut tries: Vec<PathBuf> = Vec::new();
    let settle = |tries: &mut Vec<PathBuf>, watched: &mut Watched| -> Res<()> {
        if let Some(found) = tries.pop() {
            if found.is_relative() {
                bail!("{} loads {} by a relative name, which each working directory may have another of", program.display(), found.display());
            }
            for place in tries.drain(..) {
                watched.passed(&place);
            }
        }
        Ok(())
    };
    for line in said.lines() {
        let line = line.split_once(':').map_or(line, |(_, rest)| rest).trim();
        if line.starts_with("find library=") {
            settle(&mut tries, watched)?;
        } else if let Some(file) = line.strip_prefix("trying file=") {
            tries.push(PathBuf::from(file));
        } else if let Some(paths) = line.strip_prefix("search path=") {
            let paths = paths.split('\t').next().unwrap_or_default();
            for dir in paths.split(':').filter(|dir| !dir.is_empty()) {
                if fs::metadata(dir).is_err() {
                    watched.absent.push(PathBuf::from(dir));
                }
            }
        } else if let Some(cache) = line.strip_prefix("search cache=") {
            watched.files.extend(Seen::of(Path::new(cache)));
        }
    }
    settle(&mut tries, watched)?;
    Ok(libraries)
}

/// The sources of [`relocates`]' trial: a Cactus compile in miniature. The
/// source is a build copy that says, as Cactus's do, which file it was
/// copied from; it includes one header from the tree and one from the
/// configuration, and each of the three records its own name (the source
/// both ways a compiler offers).
const TRIAL_SOURCE: &str = "#include \"file.h\"\n#include \"generated.h\"\n\
    const char *cactup_trial_file = __FILE__;\nconst char *cactup_trial_where(void) { return __builtin_FILE(); }\n\
    int cactup_trial(int x) { return twice(x) + *generated_file(); }\n";
const TRIAL_HEADER: &str = "static inline int twice(int x) { return 2 * x; }\n";
const TRIAL_GENERATED: &str = "static inline const char *generated_file(void) { return __FILE__; }\n";

/// Compile the trial sources in a tree at `root` with a configuration
/// called `config`, the way a serving cache compiles: from the
/// configuration's `scratch`, with the tree and the configuration mapped to
/// fixed names, the configuration's map last (as `key::PathMap::flags`
/// gives them). The object, if the compile succeeds.
fn trial_object(compiler: &Path, name: &OsStr, root: &Path, config: &str) -> Option<Vec<u8>> {
    let config = root.join("configs").join(config);
    let (src, build, bindings, scratch) = (root.join("src"), config.join("build"), config.join("bindings"), config.join("scratch"));
    for dir in [&src, &build, &bindings, &scratch] {
        fs::create_dir_all(dir).ok()?;
    }
    let original = src.join("file.c");
    fs::write(&original, TRIAL_SOURCE).ok()?;
    fs::write(build.join("file.c"), format!("#line 1 \"{}\"\n{TRIAL_SOURCE}", original.display())).ok()?;
    fs::write(src.join("file.h"), TRIAL_HEADER).ok()?;
    fs::write(bindings.join("generated.h"), TRIAL_GENERATED).ok()?;
    let object = build.join("file.o");
    let status = Command::new(compiler)
        .arg0(name)
        .args(["-g", "-c", "-o"])
        .arg(&object)
        .arg(build.join("file.c"))
        .arg("-I")
        .arg(&src)
        .arg("-I")
        .arg(&bindings)
        .args(super::key::trial_flags(root, &config))
        .current_dir(&scratch)
        .env("PWD", &scratch)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    status.success().then(|| fs::read(&object).ok()).flatten()
}

/// Does `compiler`, run as `name`, make one object of the same sources in
/// two different places when given the key's path map? Tried in `dir`, on
/// two small trees at paths of different length with configurations of
/// different names, with debug information on.
///
/// This is the one way to know. What a compiler does with several
/// `-ffile-prefix-map` options that match one path has differed between
/// versions, and between `__FILE__` and debug information within one
/// version; whether debug information carries a checksum of each source
/// file's bytes (and so of the path in a build copy's first line) depends
/// on the compiler and the format. A compiler without the option fails the
/// trial like one that applies it differently.
fn relocates(compiler: &Path, name: &OsStr, dir: &Path) -> bool {
    let Ok(trial) = tempfile::tempdir_in(dir) else { return false };
    let Ok(trial_dir) = fs::canonicalize(trial.path()) else { return false };
    let one = trial_object(compiler, name, &trial_dir.join("one/Cactus"), "a");
    let other = trial_object(compiler, name, &trial_dir.join("another/deeper/Cactus"), "bb");
    one.is_some() && one == other
}

/// The source of [`locale_neutral`]'s trial: bytes outside ASCII in a
/// comment, a string, a character constant's string and a wide string —
/// what a compiler that reads its source by the locale would read
/// otherwise. (Not in an identifier: GCC before 10 rejects those.)
const LOCALE_TRIAL: &[u8] = b"#include <stddef.h>\n/* d\xc3\xa9j\xc3\xa0 vu, stra\xc3\x9fe */\n\
    const char *cactup_trial_s = \"caf\xc3\xa9 \xc3\xbc \xe2\x82\xac\";\n\
    const wchar_t *cactup_trial_w = L\"caf\xc3\xa9 \xc3\xbc \xe2\x82\xac\";\n\
    int cactup_trial(void) { return sizeof(\"\xc3\xa9\") + (int)cactup_trial_w[3]; }\n";

/// Does `compiler`, run as `name`, make one object of [`LOCALE_TRIAL`] in
/// this process's locale and in the C locale (§18.8)? Each session is
/// compared with the same reference, so two sessions whose compilers pass
/// are interchangeable for it. A compiler that fails to compile it either
/// way fails the trial and keeps the locale in its keys.
fn locale_neutral(compiler: &Path, name: &OsStr, dir: &Path) -> bool {
    let Ok(trial) = tempfile::tempdir_in(dir) else { return false };
    let source = trial.path().join("locale.c");
    let object = trial.path().join("locale.o");
    if fs::write(&source, LOCALE_TRIAL).is_err() {
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
        status.success().then(|| fs::read(&object).ok()).flatten()
    };
    let session = compile(false);
    session.is_some() && session == compile(true)
}

/// Look at the compiler at `path`, run as `name`, from scratch: what it is.
/// Every file the answer was computed from goes into `files`, also when the
/// answer is "not one the cache works with". `trial_dir` is a directory for
/// [`relocates`] to try the compiler in.
fn examine(path: &Path, name: &OsStr, trial_dir: &Path, watched: &mut Watched) -> Res<Compiler> {
    // The file that runs, behind whatever links name it (`cc` -> `gcc` ->
    // `x86_64-linux-gnu-gcc-14`).
    let driver = fs::canonicalize(path).with_context(|| format!("Failed to resolve {}", path.display()))?;
    watched.files.push(Seen::of(&driver)?);
    let bytes = fs::read(&driver).with_context(|| format!("Failed to read {}", driver.display()))?;
    if !bytes.starts_with(b"\x7fELF") {
        bail!("{} is a script, not a compiler cactup can identify", path.display());
    }
    let contains = |marker: &[u8]| bytes.windows(marker.len()).any(|window| window == marker);
    let says = ask(path, &["--version"])?;

    // What the driver says, and what its own bytes say: a wrapper passes
    // `--version` on to a compiler, but it is not made of one. (The strings
    // are the title of Clang's `--help`, and two environment variables only
    // GCC's driver has a use for.)
    let family = if says.contains("clang version") && contains(b"clang LLVM compiler") {
        Family::Clang
    } else if says.contains("Free Software Foundation") && contains(b"COLLECT_GCC") && contains(b"GCC_EXEC_PREFIX") {
        match says.lines().next().is_some_and(|line| line.contains("GNU Fortran")) {
            true => Family::Gfortran,
            false => Family::Gcc,
        }
    } else {
        bail!("{} is not a compiler cactup knows (or is a wrapper around one)", path.display());
    };

    let mut hasher = Hasher::new("compiler");
    let mut specs = None;
    hasher.feed(name.as_bytes());
    hasher.feed(bytes_digest(&bytes).as_bytes());
    // The programs that do the work, each with the libraries it loads.
    let mut programs = vec![driver.clone()];
    match family {
        Family::Clang => {
            // What it says, without where it is installed. A configuration
            // file it says it reads rules it out (and is watched, so that
            // the answer changes when the file goes). This spares every
            // compile the asking; which file a given compile reads is that
            // compile's to say (`key::flags_from_elsewhere`).
            for line in says.lines() {
                match line.split_once(": ") {
                    Some(("InstalledDir", _)) => {}
                    Some(("Configuration file", file)) => {
                        let file = Path::new(file.trim());
                        watched.files.extend(Seen::of(file));
                        bail!(
                            "{} reads a configuration file ({}), which can add flags the cache does not see",
                            path.display(),
                            file.display()
                        );
                    }
                    _ => hasher.feed(line.as_bytes()),
                }
            }
        }
        Family::Gcc | Family::Gfortran => {
            hasher.feed(says.as_bytes());
            // The programs the driver hands the work to.
            let helpers: &[&str] = match family {
                Family::Gfortran => &["f951", "as"],
                _ => &["cc1", "cc1plus", "as"],
            };
            // Where the driver looks for them, in order (its own prefixes,
            // `COMPILER_PATH`'s entries, each with the target's directories
            // first): a program put at a place before the one found would
            // run instead.
            let dirs = ask(path, &["-print-search-dirs"])?;
            let dirs: Vec<PathBuf> = dirs
                .lines()
                .find_map(|line| line.strip_prefix("programs: ="))
                .with_context(|| format!("{} does not say where it looks for its programs", path.display()))?
                .split(':')
                .filter(|dir| !dir.is_empty())
                .map(PathBuf::from)
                .collect();
            for helper in helpers {
                let named = ask(path, &[&format!("-print-prog-name={helper}")])?;
                // A bare name back means "whatever PATH has": a front end
                // that is not installed (no C++), or the system assembler.
                // Passed over first: each of the driver's places.
                let found = match Path::new(named.trim()) {
                    file if file.is_absolute() => {
                        let at = dirs
                            .iter()
                            .position(|dir| dir.join(helper) == file)
                            .with_context(|| format!("{} finds {} where it does not say it looks", path.display(), file.display()))?;
                        dirs[..at].iter().for_each(|dir| watched.passed(&dir.join(helper)));
                        Some(file.to_owned())
                    }
                    name => {
                        dirs.iter().for_each(|dir| watched.passed(&dir.join(helper)));
                        search_path(name.as_os_str(), watched).ok()
                    }
                };
                hasher.feed(helper.as_bytes());
                match found {
                    Some(file) => {
                        let file = fs::canonicalize(&file).unwrap_or(file);
                        hasher.feed(file_digest(&file)?.as_bytes());
                        watched.files.push(Seen::of(&file)?);
                        programs.push(file);
                    }
                    None if *helper == "cc1plus" => hasher.feed(b"absent"),
                    None => bail!("{} names no {helper} cactup can find", path.display()),
                }
            }
            // The rules by which the driver builds their command lines: the
            // built-in ones (`-dumpspecs` ignores a specs file).
            let builtin = ask(path, &["-dumpspecs"])?;
            hasher.feed(builtin.as_bytes());
            hasher.feed(ask(path, &["-dumpmachine"])?.as_bytes());
            // A `specs` file on disk (an absolute path back means there is
            // one) overrides them, and rules the compiler out unless all it
            // changes is how GCC links. Then its bytes are part of what the
            // compiler is.
            let file = ask(path, &["-print-file-name=specs"])?;
            let file = Path::new(file.trim());
            if file.is_absolute() {
                watched.files.extend(Seen::of(file));
                let text = fs::read(file).with_context(|| format!("Failed to read {}", file.display()))?;
                if let Err(why) = super::specs::link_only(&builtin, &text, &bytes) {
                    bail!(
                        "{} reads a specs file ({}) the cache does not accept, since it can add flags the cache does not see: it {why}",
                        path.display(),
                        file.display()
                    );
                }
                hasher.feed(b"specs file");
                hasher.feed(&text);
                specs = Some(fs::canonicalize(file).with_context(|| format!("Failed to resolve {}", file.display()))?);
            }
        }
    }
    let mut libraries = Vec::new();
    for program in &programs {
        libraries.extend(loaded_libraries(program, watched)?);
    }
    libraries.sort();
    libraries.dedup();
    for library in libraries {
        let library = fs::canonicalize(&library).unwrap_or(library);
        hasher.feed(file_digest(&library)?.as_bytes());
        watched.files.push(Seen::of(&library)?);
    }
    let (relocates, locale_neutral) = match family {
        Family::Gfortran => (super::fortran::relocates(path, name, trial_dir), super::fortran::locale_neutral(path, name, trial_dir)),
        _ => (relocates(path, name, trial_dir), locale_neutral(path, name, trial_dir)),
    };
    Ok(Compiler {
        path: path.to_owned(),
        family,
        relocates,
        locale_neutral,
        id: hasher.hex(),
        specs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_programs_the_way_execvp_does() {
        let sh = find_program(OsStr::new("sh")).unwrap();
        assert!(sh.is_absolute() && sh.ends_with("sh"));
        assert_eq!(find_program(OsStr::new("/bin/sh")).unwrap(), Path::new("/bin/sh"));
        assert!(find_program(OsStr::new("no-such-compiler-anywhere")).is_err());
        // A name with a slash is a path from the working directory.
        assert_eq!(find_program(OsStr::new("./x/cc")).unwrap(), std::env::current_dir().unwrap().join("x/cc"));
    }

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        crate::objcache::make_executable(&path);
        path
    }

    #[test]
    fn a_script_is_not_a_compiler_and_that_is_remembered() {
        let tmp = tempfile::tempdir().unwrap();
        let mpicc = script(tmp.path(), "mpicc", "exec gcc \"$@\"");
        let err = identify(tmp.path(), mpicc.as_os_str()).unwrap_err().to_string();
        assert!(err.contains("is a script"), "{err}");
        // Remembered: the same answer, from the one file kept for it.
        assert_eq!(fs::read_dir(tmp.path().join("compilers")).unwrap().count(), 1);
        assert_eq!(identify(tmp.path(), mpicc.as_os_str()).unwrap_err().to_string(), err);
        // Until the file changes.
        fs::write(&mpicc, "#!/bin/sh\nexec clang \"$@\"\n# changed\n").unwrap();
        assert!(identify(tmp.path(), mpicc.as_os_str()).unwrap_err().to_string().contains("is a script"));
    }

    /// What a driver says of itself goes into its identity, so it is asked
    /// in English whatever the session's language: the variables that pick
    /// a message catalog are set or cleared on the way in.
    #[test]
    fn a_driver_is_asked_in_english() {
        let tmp = tempfile::tempdir().unwrap();
        let driver = script(tmp.path(), "cc", r#"echo "LC_MESSAGES=$LC_MESSAGES LANGUAGE=${LANGUAGE-unset} $1""#);
        assert_eq!(ask(&driver, &["--version"]).unwrap(), "LC_MESSAGES=C LANGUAGE=unset --version\n");
    }

    /// The real GCC of this host, if it has one.
    fn gcc() -> Option<PathBuf> {
        let gcc = find_program(OsStr::new("gcc")).ok()?;
        ask(&gcc, &["--version"]).is_ok_and(|says| says.contains("Free Software Foundation")).then_some(gcc)
    }

    #[test]
    fn identifies_gcc_and_remembers_it() {
        let Some(gcc) = gcc() else {
            eprintln!("skipped: no GCC on this host");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let first = identify(tmp.path(), OsStr::new("gcc")).unwrap();
        assert_eq!(first.family, Family::Gcc);
        assert_eq!(first.path, gcc);
        assert!(first.relocates && first.id.len() == 64);
        // GCC reads its source as UTF-8 whatever the locale (§18.8).
        assert!(first.locale_neutral);

        // Remembered: one file, and the same answer from it.
        let memos: Vec<_> = fs::read_dir(tmp.path().join("compilers")).unwrap().collect();
        assert_eq!(memos.len(), 1);
        assert_eq!(identify(tmp.path(), OsStr::new("gcc")).unwrap(), first);
        // By path it is the same compiler.
        assert_eq!(identify(tmp.path(), gcc.as_os_str()).unwrap().id, first.id);

        // A memo that no longer matches what is on disk is not trusted.
        let memo = memos[0].as_ref().unwrap().path();
        let stale = fs::read_to_string(&memo).unwrap().replace(&first.id, &"0".repeat(64)).replace("size = ", "size = 1");
        fs::write(&memo, stale).unwrap();
        assert_eq!(identify(tmp.path(), OsStr::new("gcc")).unwrap().id, first.id);

        // g++ is another driver with the same back ends: another identity.
        if find_program(OsStr::new("g++")).is_ok() {
            assert_ne!(identify(tmp.path(), OsStr::new("g++")).unwrap().id, first.id);
        }
        // So is the same file under another name: a compiler may behave by
        // the name it is run by.
        let alias = tmp.path().join("cc-by-another-name");
        std::os::unix::fs::symlink(&gcc, &alias).unwrap();
        assert_ne!(identify(tmp.path(), alias.as_os_str()).unwrap().id, first.id);
    }

    /// A copy of this host's GCC driver installed under `prefix` (its back
    /// ends linked in where it looks for them), with `specs`, if given, as
    /// the specs file it finds. GCC looks for both relative to where its
    /// driver is, as a site-built GCC does.
    fn gcc_under(gcc: &Path, prefix: &Path, specs: Option<&str>) -> PathBuf {
        let cc1 = PathBuf::from(ask(gcc, &["-print-prog-name=cc1"]).unwrap().trim());
        let version = cc1.parent().unwrap();
        let triple = ask(gcc, &["-dumpmachine"]).unwrap().trim().to_owned();
        let version_name = version.file_name().unwrap();
        let libexec = prefix.join("libexec/gcc").join(&triple);
        let lib = prefix.join("lib/gcc").join(&triple).join(version_name);
        for dir in [&prefix.join("bin"), &libexec, &lib] {
            fs::create_dir_all(dir).unwrap();
        }
        std::os::unix::fs::symlink(version, libexec.join(version_name)).unwrap();
        let driver = prefix.join("bin/gcc");
        fs::copy(fs::canonicalize(gcc).unwrap(), &driver).unwrap();
        crate::objcache::make_executable(&driver);
        if let Some(specs) = specs {
            fs::write(lib.join("specs"), specs).unwrap();
        }
        driver
    }

    #[test]
    fn a_gcc_whose_specs_file_only_changes_the_link_is_one_the_cache_works_with() {
        let Some(gcc) = gcc() else {
            eprintln!("skipped: no GCC on this host");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let builtin = ask(&gcc, &["-dumpspecs"]).unwrap();
        // qbd's specs file, made the way its administrators made it.
        let sneaky = builtin.replacen("*cc1:\n", "*cc1:\n-DSNEAKY ", 1);
        let qbd = builtin.replacen("*link_libgcc:\n%D\n", "*link_libgcc:\n%(link_libgcc_rpath) %D\n", 1)
            + "*link_libgcc_rpath:\n-rpath /opt/gcc/lib64\n\n";
        assert!(qbd.contains("%(link_libgcc_rpath) %D") && sneaky.contains("-DSNEAKY"), "not the sections expected");

        let plain = gcc_under(&gcc, &tmp.path().join("plain"), None);
        let plain = identify(tmp.path(), plain.as_os_str()).unwrap();
        assert_eq!(plain.specs, None);

        let linking = gcc_under(&gcc, &tmp.path().join("linking"), Some(&qbd));
        let linking = identify(tmp.path(), linking.as_os_str()).unwrap();
        let file = tmp.path().join("linking/lib/gcc").join(ask(&gcc, &["-dumpmachine"]).unwrap().trim());
        assert!(linking.specs.as_ref().is_some_and(|specs| specs.starts_with(fs::canonicalize(file).unwrap())), "{linking:?}");
        assert_ne!(linking.id, plain.id, "the specs file is part of what the compiler is");

        let compiling = gcc_under(&gcc, &tmp.path().join("compiling"), Some(&sneaky));
        let err = identify(tmp.path(), compiling.as_os_str()).unwrap_err().to_string();
        assert!(err.contains("reads a specs file") && err.contains("changes cc1"), "{err}");
    }

    /// A compiler whose objects follow the locale fails the trial: here a
    /// stand-in that writes its "object" by the locale it was run in.
    #[test]
    fn a_compiler_that_reads_the_locale_keeps_it_in_its_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let by_locale = script(tmp.path(), "by-locale", "while [ $# -gt 1 ]; do [ \"$1\" = -o ] && out=$2; shift; done; echo \"${LC_ALL-}${LANG-}\" > \"$out\"");
        let fixed = script(tmp.path(), "fixed", "while [ $# -gt 1 ]; do [ \"$1\" = -o ] && out=$2; shift; done; echo same > \"$out\"");
        // Run from a child, so that the session's locale is one this test
        // sets, and not C already.
        let outcome = |compiler: &Path| {
            let out = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "objcache::identity::tests::child_tries_the_locale", "--nocapture", "--ignored"])
                .env("CACTUP_LOCALE_TRIAL", compiler)
                .env("LANG", "de_DE.UTF-8")
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        assert!(outcome(&by_locale).contains("neutral: false"), "{}", outcome(&by_locale));
        assert!(outcome(&fixed).contains("neutral: true"), "{}", outcome(&fixed));
    }

    /// Run only by `a_compiler_that_reads_the_locale_keeps_it_in_its_keys`.
    #[test]
    #[ignore]
    fn child_tries_the_locale() {
        let Some(compiler) = std::env::var_os("CACTUP_LOCALE_TRIAL") else { return };
        let tmp = tempfile::tempdir().unwrap();
        let compiler = PathBuf::from(compiler);
        println!("neutral: {}", locale_neutral(&compiler, compiler.file_name().unwrap(), tmp.path()));
    }

    #[test]
    fn tries_whether_the_path_map_holds() {
        let Some(gcc) = gcc() else {
            eprintln!("skipped: no GCC on this host");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        assert!(relocates(&gcc, OsStr::new("gcc"), tmp.path()));
        // Stand-ins that are GCC except for one thing each: applying the
        // first matching map instead of the last (the maps handed over in
        // the other order), ignoring the maps, not compiling at all.
        let split = "maps=; rest=; for a; do case $a in -ffile-prefix-map=*) maps=\"$a $maps\";; *) rest=\"$rest $a\";; esac; done";
        let first_wins = script(tmp.path(), "first-wins", &format!("{split}\nexec {} $rest $maps", gcc.display()));
        let no_map = script(tmp.path(), "no-map", &format!("{split}\nexec {} $rest", gcc.display()));
        for stand_in in [first_wins, no_map, script(tmp.path(), "broken", "exit 1"), PathBuf::from("/nonexistent/cc")] {
            assert!(!relocates(&stand_in, stand_in.file_name().unwrap(), tmp.path()), "{}", stand_in.display());
        }
        // Nothing of the trials is left behind: only the three scripts.
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 3);
    }

    #[test]
    fn a_relative_path_entry_finds_what_execvp_finds() {
        let tmp = tempfile::tempdir().unwrap();
        script(tmp.path(), "shadowcc", "exit 0");
        // `find_program` reads PATH and the working directory of this
        // process, which other tests share: ask a child, in its own.
        let out = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "objcache::identity::tests::child_resolves_shadowcc", "--nocapture", "--ignored"])
            .current_dir(tmp.path())
            .env("PATH", format!(".:{}", std::env::var("PATH").unwrap()))
            .output()
            .unwrap();
        let found = String::from_utf8_lossy(&out.stdout);
        let expected = fs::canonicalize(tmp.path()).unwrap().join("shadowcc");
        assert!(found.contains(&format!("found {}", expected.display())), "{found}{}", String::from_utf8_lossy(&out.stderr));
    }

    /// A program or library put where the search passed over before what
    /// it found is another compiler: the remembered identity does not hold.
    /// (Tried for real: an assembler appearing on `PATH` mid-build assembled
    /// objects stored under the real one's identity.)
    #[test]
    fn what_appears_before_a_program_or_library_found_is_seen() {
        if gcc().is_none() {
            eprintln!("skipped: no GCC on this host");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("bin")).unwrap();
        // `identify` reads the environment and the working directory of this
        // process, which other tests share: ask a child, in its own.
        let out = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "objcache::identity::tests::child_watches_places", "--nocapture", "--ignored"])
            .current_dir(tmp.path())
            .env("PATH", format!("{}:{}", tmp.path().join("bin").display(), std::env::var("PATH").unwrap()))
            .env("LD_LIBRARY_PATH", ":/nonexistent-cactup-test")
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stdout);
        assert!(said.contains("assembler seen") && said.contains("library seen"), "{said}{}", String::from_utf8_lossy(&out.stderr));
    }

    /// Run only by `what_appears_before_a_program_or_library_found_is_seen`.
    #[test]
    #[ignore]
    fn child_watches_places() {
        let memo = tempfile::tempdir().unwrap();
        let first = identify(memo.path(), OsStr::new("gcc")).unwrap();
        assert_eq!(identify(memo.path(), OsStr::new("gcc")).unwrap(), first);
        // An assembler first on `PATH`.
        script(Path::new("bin"), "as", "exec /usr/bin/as \"$@\"");
        if identify(memo.path(), OsStr::new("gcc")).is_ok_and(|now| now.id != first.id) {
            println!("assembler seen");
        }
        fs::remove_file("bin/as").unwrap();
        assert_eq!(identify(memo.path(), OsStr::new("gcc")).unwrap(), first);
        // A library in the working directory, where the empty entry of
        // `LD_LIBRARY_PATH` has the loader look first.
        let cc1 = PathBuf::from(ask(&first.path, &["-print-prog-name=cc1"]).unwrap().trim());
        let mut tried = Watched { files: &mut Vec::new(), absent: &mut Vec::new() };
        let libraries = loaded_libraries(&cc1, &mut tried).unwrap();
        let library = libraries.iter().find(|library| !library.to_string_lossy().contains("ld-linux") && !library.to_string_lossy().contains("libc.so")).unwrap();
        fs::copy(library, library.file_name().unwrap()).unwrap();
        if identify(memo.path(), OsStr::new("gcc")).is_err_and(|e| format!("{e:#}").contains("by a relative name")) {
            println!("library seen");
        }
    }

    /// Run only by `a_relative_path_entry_finds_what_execvp_finds`.
    #[test]
    #[ignore]
    fn child_resolves_shadowcc() {
        if let Ok(found) = find_program(OsStr::new("shadowcc")) {
            println!("found {}", fs::canonicalize(found).unwrap().display());
        }
    }
}
