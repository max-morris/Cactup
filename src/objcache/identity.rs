//! Which compiler a command line runs: the part of a cache key that says
//! "the same program would run", by content and not by name.
//!
//! A compiler is identified by the bytes of what actually does the work —
//! the driver the command names, for GCC the back ends and assembler it
//! runs, and the shared libraries each of those loads (a distribution's
//! Clang is a small driver in front of `libclang-cpp.so`) — plus what it
//! says about itself (version, target, specs). Its
//! path is not part of it, so two installations of the same compiler agree;
//! its modification time is not either, so one compiler copied twice does.
//!
//! Only a compiler cactup can identify *as itself* is cached. A wrapper
//! (`mpicc`, a Cray `cc`, a site script) adds flags and picks a compiler by
//! rules of its own; it passes `--version` through to the compiler, and so
//! looks like one. The driver's own bytes have to say what it is.
//!
//! Hashing a compiler takes a moment (GCC's `cc1plus` is tens of megabytes)
//! and every compile of a build asks, so the answer is kept for the length
//! of one build attempt, in `<attempt>/cc/compilers/`, and reused as long as
//! every file it was computed from still looks the same (size, change time,
//! inode).

use super::hash::{bytes_digest, file_digest, Hasher};
use crate::Res;
use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The compiler families the cache has an argument reader for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Family {
    Gcc,
    Clang,
}

/// An identified compiler.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Compiler {
    /// The program the command line names, as an absolute path.
    pub path: PathBuf,
    pub family: Family,
    /// Major and minor version, for what the family can do from which
    /// version on.
    pub version: (u32, u32),
    /// The digest that stands for this compiler in a key.
    pub id: String,
}

/// One file an identity was computed from, as it looked then.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Seen {
    path: PathBuf,
    size: u64,
    ctime: i64,
    ctime_nsec: i64,
    ino: u64,
    dev: u64,
}

impl Seen {
    fn of(path: &Path) -> Res<Self> {
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
    compiler: Compiler,
    /// The environment that decides which helper programs the driver runs,
    /// as it was.
    env: String,
    files: Vec<Seen>,
}

/// The variables that decide which back ends and libraries a driver picks
/// up. A change in one of them may mean another compiler under the same
/// driver.
const HELPER_ENV: &[&str] = &["PATH", "COMPILER_PATH", "GCC_EXEC_PREFIX", "LD_LIBRARY_PATH", "LD_PRELOAD"];

fn helper_env() -> String {
    let mut hasher = Hasher::new("compiler-env");
    for name in HELPER_ENV {
        hasher.feed(name.as_bytes());
        hasher.feed(std::env::var_os(name).unwrap_or_default().as_bytes());
    }
    hasher.hex()
}

/// The program the shell's `PATH` search finds for `program`, as an absolute
/// path: the first executable regular file of that name.
pub fn find_program(program: &OsStr) -> Res<PathBuf> {
    if program.as_bytes().contains(&b'/') {
        return std::path::absolute(program).with_context(|| format!("Failed to resolve {}", program.to_string_lossy()));
    }
    let executable = |path: &PathBuf| path.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    let path = std::env::var_os("PATH").context("PATH is not set")?;
    let found = std::env::split_paths(&path).filter(|dir| dir.is_absolute()).map(|dir| dir.join(program)).find(executable);
    found.with_context(|| format!("{} is not on PATH", program.to_string_lossy()))
}

/// Identify the compiler `program` names, reusing what an earlier compile of
/// this build attempt found if nothing has changed since. `Err` says why
/// this is not a compiler the cache works with.
pub fn identify(cc_dir: &Path, program: &OsStr) -> Res<Compiler> {
    let path = find_program(program)?;
    let memo_dir = cc_dir.join("compilers");
    let memo = memo_dir.join(format!("{}.toml", bytes_digest(path.as_os_str().as_bytes())));
    let env = helper_env();
    if let Some(remembered) = fs::read_to_string(&memo).ok().and_then(|text| toml::from_str::<Remembered>(&text).ok())
        && remembered.env == env
        && remembered.files.iter().all(|seen| Seen::of(&seen.path).is_ok_and(|now| now == *seen))
    {
        return Ok(remembered.compiler);
    }

    let (compiler, files) = examine(&path)?;
    // Best-effort: without it the next compile just looks again. Written
    // whole and moved into place, since every compile of the build reads it.
    let remembered = Remembered { compiler: compiler.clone(), env, files };
    if let Ok(text) = toml::to_string(&remembered)
        && fs::create_dir_all(&memo_dir).is_ok()
        && let Ok(mut temp) = tempfile::NamedTempFile::new_in(&memo_dir)
    {
        use std::io::Write;
        if temp.write_all(text.as_bytes()).is_ok() {
            let _ = temp.persist(&memo);
        }
    }
    Ok(compiler)
}

/// Run `program` with `args` and return its standard output, if it succeeds.
fn ask(program: &Path, args: &[&str]) -> Res<String> {
    let out = Command::new(program)
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
fn loaded_libraries(program: &Path) -> Vec<PathBuf> {
    let out = Command::new(program)
        .arg("--version")
        .env("LD_TRACE_LOADED_OBJECTS", "1")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let listed = out.map(|out| String::from_utf8_lossy(&out.stdout).into_owned()).unwrap_or_default();
    // `\tlibisl.so.23 => /lib/x86_64-linux-gnu/libisl.so.23 (0x00007f…)`
    let library = |line: &str| Some(PathBuf::from(line.split_once(" => ")?.1.rsplit_once(" (")?.0));
    listed.lines().filter_map(library).filter(|path| path.is_absolute()).collect()
}

/// `major.minor` at the start of `text`.
fn version_of(text: &str) -> Option<(u32, u32)> {
    let mut numbers = text.trim().split(|c: char| !c.is_ascii_digit()).map(str::parse);
    Some((numbers.next()?.ok()?, numbers.next()?.ok()?))
}

/// Look at the compiler at `path` from scratch: what it is, and every file
/// its identity was computed from.
fn examine(path: &Path) -> Res<(Compiler, Vec<Seen>)> {
    // The file that runs, behind whatever links name it (`cc` -> `gcc` ->
    // `x86_64-linux-gnu-gcc-14`).
    let driver = fs::canonicalize(path).with_context(|| format!("Failed to resolve {}", path.display()))?;
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
        if says.lines().next().is_some_and(|line| line.contains("GNU Fortran")) {
            bail!("Fortran is not cached yet");
        }
        Family::Gcc
    } else {
        bail!("{} is not a compiler cactup knows (or is a wrapper around one)", path.display());
    };

    let mut hasher = Hasher::new("compiler");
    let mut files = vec![Seen::of(&driver)?];
    hasher.feed(says.as_bytes());
    hasher.feed(bytes_digest(&bytes).as_bytes());
    // The programs that do the work, each with the libraries it loads.
    let mut programs = vec![driver.clone()];
    let version = match family {
        Family::Clang => {
            let after = says.split("clang version").nth(1).context("clang did not print its version")?;
            version_of(after).context("clang printed a version cactup cannot read")?
        }
        Family::Gcc => {
            // The programs the driver hands the work to, and the rules by
            // which it builds their command lines.
            for helper in ["cc1", "cc1plus", "as"] {
                let named = ask(path, &[&format!("-print-prog-name={helper}")])?;
                // A bare name back means "whatever PATH has": a front end
                // that is not installed (no C++), or the system assembler.
                let found = match Path::new(named.trim()) {
                    file if file.is_absolute() => Some(file.to_owned()),
                    name => find_program(name.as_os_str()).ok(),
                };
                hasher.feed(helper.as_bytes());
                match found {
                    Some(file) => {
                        let file = fs::canonicalize(&file).unwrap_or(file);
                        hasher.feed(file_digest(&file)?.as_bytes());
                        files.push(Seen::of(&file)?);
                        programs.push(file);
                    }
                    None if helper == "cc1plus" => hasher.feed(b"absent"),
                    None => bail!("{} names no {helper} cactup can find", path.display()),
                }
            }
            for question in ["-dumpspecs", "-dumpmachine"] {
                hasher.feed(ask(path, &[question])?.as_bytes());
            }
            let full = ask(path, &["-dumpfullversion"]).or_else(|_| ask(path, &["-dumpversion"]))?;
            version_of(&format!("{}.0", full.trim())).context("gcc printed a version cactup cannot read")?
        }
    };
    let mut libraries: Vec<PathBuf> = programs.iter().flat_map(|program| loaded_libraries(program)).collect();
    libraries.sort();
    libraries.dedup();
    for library in libraries {
        let library = fs::canonicalize(&library).unwrap_or(library);
        hasher.feed(file_digest(&library)?.as_bytes());
        files.push(Seen::of(&library)?);
    }
    Ok((Compiler { path: path.to_owned(), family, version, id: hasher.hex() }, files))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_versions() {
        assert_eq!(version_of("14.2.0"), Some((14, 2)));
        assert_eq!(version_of(" 19.1.7 (3+b1)\nTarget: x"), Some((19, 1)));
        assert_eq!(version_of("8.0"), Some((8, 0)));
        assert_eq!(version_of("unknown"), None);
    }

    #[test]
    fn finds_programs_the_way_a_shell_does() {
        let sh = find_program(OsStr::new("sh")).unwrap();
        assert!(sh.is_absolute() && sh.ends_with("sh"));
        assert_eq!(find_program(OsStr::new("/bin/sh")).unwrap(), Path::new("/bin/sh"));
        assert!(find_program(OsStr::new("no-such-compiler-anywhere")).is_err());
    }

    #[test]
    fn a_script_is_not_a_compiler() {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("mpicc");
        fs::write(&script, "#!/bin/sh\nexec gcc \"$@\"\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let err = identify(tmp.path(), script.as_os_str()).unwrap_err().to_string();
        assert!(err.contains("is a script"), "{err}");
    }

    /// With a real GCC, if this host has one.
    #[test]
    fn identifies_gcc_and_remembers_it() {
        let Ok(gcc) = find_program(OsStr::new("gcc")) else {
            eprintln!("skipped: no gcc on this host");
            return;
        };
        if !ask(&gcc, &["--version"]).is_ok_and(|says| says.contains("Free Software Foundation")) {
            eprintln!("skipped: gcc here is not GCC");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let first = identify(tmp.path(), OsStr::new("gcc")).unwrap();
        assert_eq!(first.family, Family::Gcc);
        assert_eq!(first.path, gcc);
        assert!(first.version.0 >= 4 && first.id.len() == 64);

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
    }
}
