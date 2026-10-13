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
use super::key::{digest_output, flags_from_elsewhere, read_files, Named, PathMap, PREPROCESSOR};
use std::sync::atomic::Ordering;
use crate::Res;
use anyhow::{bail, Context};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// What `nvcc -dryrun` says: the environment it sets for the programs it
/// runs (`PATH`, `LD_LIBRARY_PATH`, `CICC_PATH`, `NVVMIR_LIBRARY_DIR`, …),
/// and the commands, each as its words.
#[derive(Debug, Default, PartialEq)]
pub struct DryRun {
    pub env: Vec<(String, String)>,
    pub commands: Vec<Vec<String>>,
}

impl DryRun {
    /// Read what `nvcc -dryrun` wrote (to standard error): each line it
    /// would run is `#$ ` and the line. An assignment (`NAME=value`) sets the
    /// environment of what follows; any other line is a command, split as
    /// the shell would split it, with `$NAME` taken from that environment.
    pub fn read(said: &str) -> Self {
        let mut run = Self::default();
        for line in said.lines().filter_map(|line| line.strip_prefix("#$ ")) {
            let name_end = line.find('=').filter(|at| {
                let name = &line[..*at];
                !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            });
            match name_end {
                Some(at) => {
                    let value = line[at + 1..].trim_end();
                    run.env.retain(|(name, _)| name != &line[..at]);
                    run.env.push((line[..at].to_owned(), value.to_owned()));
                }
                None => {
                    let words = shell_words(line, |name| run.get(name).map(str::to_owned));
                    if !words.is_empty() {
                        run.commands.push(words);
                    }
                }
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
                ["cudafe++", "cicc", "ptxas", "fatbinary", "nvlink", "bin2c"].contains(&base)
            })
            .collect();
        programs.sort_unstable();
        programs.dedup();
        programs
    }
}

/// `line` split into words as a POSIX shell splits it: blanks separate,
/// double and single quotes join, a backslash escapes, `$NAME` and
/// `${NAME}` are replaced by what `get` gives (outside single quotes).
pub fn shell_words(line: &str, get: impl Fn(&str) -> Option<String>) -> Vec<String> {
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
            (_, '\\') => {
                if let Some(next) = chars.next() {
                    word.push(next);
                    any = true;
                }
            }
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
                if braced && chars.peek() == Some(&'}') {
                    chars.next();
                }
                match name.is_empty() {
                    true => word.push('$'),
                    false => word.push_str(&get(&name).unwrap_or_default()),
                }
                any = true;
            }
            (None, c) if c.is_whitespace() => {
                if any {
                    words.push(std::mem::take(&mut word));
                }
                any = false;
            }
            (_, c) => {
                word.push(c);
                any = true;
            }
        }
    }
    if any {
        words.push(word);
    }
    words
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

/// Does `nvcc` (at `path`, run as `name`) make one object of the same CUDA
/// source in two places, under the path map's flags as the cache gives
/// them ([`map_flags`])? Tried in `dir`, with its default host compiler and
/// the relocatable device code Cactus asks for, as a GCC is tried
/// (`identity::relocates`): the module ID `cudafe++` makes is a hash of the
/// source's name, which the map's flags fix (tried).
pub(super) fn relocates(path: &Path, name: &OsStr, dir: &Path, isolate: bool) -> bool {
    let Ok(trial) = tempfile::Builder::new().prefix("cuda-trial-").tempdir_in(dir) else { return false };
    // By its physical path, and with `PWD` naming it, as the GCC trial: the
    // host compiler records `$PWD` where it names the working directory
    // (tried: a `/work` link on qbd left the physical path unmapped).
    let Ok(trial_dir) = fs::canonicalize(trial.path()) else { return false };
    // With an entity of internal linkage, whose name `cudafe++` makes from
    // its process ID (decision 24).
    let text = "namespace { __device__ int hidden; }\n__device__ int twice(int v) { return 2 * v + hidden; }\n__global__ void k(int *p) { p[threadIdx.x] = twice(p[threadIdx.x]); }\nvoid launch(int *p) { k<<<1, 32>>>(p); }\n";
    // Laid out as Cactus lays out a compile (the source copied into the
    // configuration's build directory, run from its scratch directory),
    // in trees and configurations of names of other lengths: the map has
    // the configuration directory inside the root to get right.
    let mut objects = Vec::new();
    for (place, config) in [("one/Cactus", "sim"), ("another/deeper/Cactus", "simulation")] {
        let root = trial_dir.join(place);
        let config = root.join("configs").join(config);
        let (build, scratch) = (config.join("build/Thorn"), config.join("scratch"));
        let source = build.join("trial.cu");
        let made = [&build, &scratch, &config.join("bindings/include")].iter().all(|dir| fs::create_dir_all(dir).is_ok());
        if !made || fs::write(&source, text).is_err() {
            return false;
        }
        let object = build.join("trial.cu.o");
        let mut args: Vec<OsString> = ["-x", "cu", "-g", "-O2", "--relocatable-device-code=true", "--objdir-as-tempdir", "-c", "-o"].map(OsString::from).to_vec();
        args.push(object.clone().into_os_string());
        args.push(source.clone().into_os_string());
        args.push(OsString::from("-I"));
        args.push(config.join("bindings/include").into_os_string());
        args.extend(map_flags(&source, &PathMap::for_trial(&root, &config)));
        let mut command = match isolate {
            true => isolated(path, name, &args),
            false => {
                let mut command = Command::new(path);
                command.arg0(name).args(&args);
                command
            }
        };
        command
            .current_dir(&scratch)
            .env("PWD", &scratch)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        match command.output() {
            Ok(out) if out.status.success() => {}
            Ok(out) => {
                identity::trial_says(&format!("{} failed the trial compile ({}): {}", path.display(), out.status, String::from_utf8_lossy(&out.stderr)));
                return false;
            }
            Err(e) => {
                identity::trial_says(&format!("{} could not run the trial compile: {e}", path.display()));
                return false;
            }
        }
        match fs::read(&object) {
            Ok(object) => objects.push(object),
            Err(_) => return false,
        }
    }
    if objects[0] != objects[1] {
        identity::trial_says(&format!("{} made two objects of the trial in two places", path.display()));
    }
    objects[0] == objects[1]
}

/// Can a program be started in a user and PID namespace of its own here
/// ([`isolated`])? Tried once, by starting `true` so.
pub fn isolation() -> bool {
    std::env::current_exe().is_ok_and(|me| isolation_by(&me))
}

/// [`isolation`], with `me` the cactup that is the namespace's init.
fn isolation_by(me: &Path) -> bool {
    let Ok(truth) = identity::find_program(OsStr::new("true")) else { return false };
    isolated_by(me, &truth, OsStr::new("true"), &[])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// `program`, run as `arg0` with `args`, in a user and PID namespace of its
/// own (decision 24), through this cactup as the namespace's init
/// ([`init`]). The namespaces are made in the child between fork and exec
/// (`unshare(2)` by `rustix`: a raw system call, no C library): a user
/// namespace mapping this user and group to themselves (needs no
/// privilege), and a PID namespace, which the next process the child starts
/// is the first of.
pub fn isolated(program: &Path, arg0: &OsStr, args: &[OsString]) -> Command {
    isolated_by(&std::env::current_exe().unwrap_or_else(|_| PathBuf::from("cactup")), program, arg0, args)
}

/// [`isolated`], with `me` the cactup that is the namespace's init.
fn isolated_by(me: &Path, program: &Path, arg0: &OsStr, args: &[OsString]) -> Command {
    let (uid, gid) = (rustix::process::getuid().as_raw(), rustix::process::getgid().as_raw());
    // Made before the fork: nothing is allocated after it.
    let uid_map = format!("{uid} {uid} 1\n").into_bytes();
    let gid_map = format!("{gid} {gid} 1\n").into_bytes();
    let mut command = Command::new(me);
    command.arg(super::INIT_VERB).arg(program).arg(arg0).args(args);
    let write = |path: &std::ffi::CStr, bytes: &[u8]| -> std::io::Result<()> {
        let file = rustix::fs::open(path, rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::CLOEXEC, rustix::fs::Mode::empty())?;
        rustix::io::write(&file, bytes)?;
        Ok(())
    };
    // SAFETY: runs in the forked child before exec, where only
    // async-signal-safe work may be done: raw system calls (rustix makes
    // them without the C library) on buffers made before the fork. The
    // child has one thread, which `unshare` of a user namespace needs, and
    // no file descriptor table is unshared.
    unsafe {
        command.pre_exec(move || {
            rustix::thread::unshare_unsafe(rustix::thread::UnshareFlags::NEWUSER | rustix::thread::UnshareFlags::NEWPID)?;
            write(c"/proc/self/setgroups", b"deny")?;
            write(c"/proc/self/uid_map", &uid_map)?;
            write(c"/proc/self/gid_map", &gid_map)?;
            Ok(())
        });
    }
    command
}

/// What [`init`] tells the process it starts that it is the namespace's
/// first.
const FIRST_ARG: &str = "--first";

/// The first process of the PID namespace, while it runs (to end it).
static FIRST: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// Run a CUDA compile's program in its PID namespace: `<program> <arg0>
/// <args…>`, as [`isolated`] starts this. Outside the namespace (the
/// process [`isolated`] made it in), start this again, which is the
/// namespace's first process, and end as it ends; a stop signal kills it,
/// and with it everything in the namespace. As the first process, start the
/// program, reap whatever it leaves (the first process adopts every
/// orphan), and end as it ended: a signal it died of becomes the exit
/// status `128 + n`, since the first process of a PID namespace cannot be
/// ended by a signal it has no handler for.
pub fn init(args: impl Iterator<Item = OsString>) -> i32 {
    let mut args: Vec<OsString> = args.collect();
    let first = args.first().is_some_and(|arg| arg == FIRST_ARG);
    if first {
        args.remove(0);
        // Only the namespace's first process may go on: anything else would
        // start itself again, and again.
        if rustix::process::getpid().as_raw_nonzero().get() != 1 {
            eprintln!("cactup: the init of a CUDA compile is not the first process of its namespace");
            return 127;
        }
    } else {
        let Ok(me) = std::env::current_exe() else { return 127 };
        let child = match Command::new(me).arg(super::INIT_VERB).arg(FIRST_ARG).args(&args).spawn() {
            Ok(child) => child,
            Err(e) => {
                eprintln!("cactup: cannot start a CUDA compile in its namespace: {e}");
                return 127;
            }
        };
        FIRST.store(child.id() as i32, Ordering::SeqCst);
        for signal in [libc_signal::SIGHUP, libc_signal::SIGINT, libc_signal::SIGQUIT, libc_signal::SIGTERM] {
            let on_signal = || {
                if let Some(first) = rustix::process::Pid::from_raw(FIRST.load(Ordering::SeqCst)) {
                    let _ = rustix::process::kill_process(first, rustix::process::Signal::KILL);
                }
            };
            // SAFETY: the handler reads an atomic and makes a raw `kill`
            // system call: async-signal-safe.
            let _ = unsafe { signal_hook::low_level::register(signal, on_signal) };
        }
        return wait_for(child.id() as i32);
    }
    let mut args = args.into_iter();
    let (Some(program), Some(arg0)) = (args.next(), args.next()) else {
        eprintln!("cactup: the init of a CUDA compile was run without a program");
        return 2;
    };
    match Command::new(&program).arg0(&arg0).args(args).spawn() {
        Ok(child) => wait_for(child.id() as i32),
        Err(e) => {
            eprintln!("cactup: {}: {e}", Path::new(&program).display());
            127
        }
    }
}

/// Signal numbers, without the C library.
mod libc_signal {
    pub const SIGHUP: i32 = 1;
    pub const SIGINT: i32 = 2;
    pub const SIGQUIT: i32 = 3;
    pub const SIGTERM: i32 = 15;
}

/// Wait for the process `pid`, reaping any other child on the way, and say
/// how it ended as an exit status.
fn wait_for(pid: i32) -> i32 {
    let ours = rustix::process::Pid::from_raw(pid);
    loop {
        match rustix::process::waitpid(None, rustix::process::WaitOptions::empty()) {
            Ok(Some((pid, status))) if Some(pid) == ours => {
                return match (status.exit_status(), status.terminating_signal()) {
                    (Some(code), _) => code,
                    (None, Some(signal)) => 128 + signal,
                    (None, None) => 1,
                };
            }
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => {}
            Err(_) => return 1,
        }
    }
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
    let program = OsStr::new(&first[0]);
    let found = match program.as_bytes().contains(&b'/') {
        true => PathBuf::from(program),
        false => {
            let search = run.get("PATH").context("nvcc does not say where it finds the host compiler")?;
            identity::find_along(program, OsStr::new(search))?
                .with_context(|| format!("nvcc would run {}, which is not on its PATH", program.to_string_lossy()))?
        }
    };
    let (host, watch) = identity::identify_watching(cc_dir, found.as_os_str())?;
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
}

/// The key's text and files for a CUDA compile: what its preprocessor
/// passes print and read.
pub struct Read {
    pub text: String,
    pub text_bytes: u64,
    pub files: String,
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
pub fn key(run: &DryRun, host: Compiler, compile: &Compile, map: Option<&PathMap>) -> Res<Read> {
    let passes: Vec<Vec<String>> = run.preprocessor_passes().cloned().collect();
    let cuda = Cuda { host, passes, env: run.env.clone() };
    let (text, text_bytes, named) = run_passes(&cuda, map)?;
    let mut named = named;
    // The source itself, whatever the markers call it.
    let source = compile.source.as_os_str().as_bytes();
    named.insert((map.map_or_else(|| source.to_vec(), |map| map.apply(source)), compile.source.clone()), true);
    let (files, seen) = read_files(&named, map)?;
    Ok(Read { text, text_bytes, files, count: named.len(), seen, cuda })
}

/// Run the passes of `cuda` again, and digest them: the text of each pass in
/// turn, and every file any of them names.
fn run_passes(cuda: &Cuda, map: Option<&PathMap>) -> Res<(String, u64, Named)> {
    let mut text = Hasher::new("cuda passes");
    let (mut bytes, mut named) = (0u64, Named::new());
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
        let (pass_text, pass_bytes, pass_named) = read?;
        if !status.context("Failed to wait for nvcc's preprocessor")?.success() {
            bail!("nvcc's preprocessor failed");
        }
        flags_from_elsewhere(cuda.host.family, cuda.host.specs.as_deref(), &said).map_err(anyhow::Error::msg)?;
        text.feed(pass_text.as_bytes());
        bytes += pass_bytes;
        for (name, entered) in pass_named {
            *named.entry(name).or_default() |= entered;
        }
    }
    Ok((text.hex(), bytes, named))
}

/// Does the key of a CUDA compile still hold after it: the same passes print
/// the same text, from the same files, which look as they did?
pub fn still_holds(cuda: &Cuda, compile: &Compile, map: Option<&PathMap>, text: &str, files: &str, seen: &str) -> bool {
    let Ok((now_text, _, mut named)) = run_passes(cuda, map) else { return false };
    let source = compile.source.as_os_str().as_bytes();
    named.insert((map.map_or_else(|| source.to_vec(), |map| map.apply(source)), compile.source.clone()), true);
    now_text == text && read_files(&named, map).is_ok_and(|(now_files, now_seen)| now_files == files && now_seen == seen)
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

    /// The built cactup, where `cargo test` has built it beside the tests.
    fn cactup() -> Option<PathBuf> {
        let me = std::env::current_exe().ok()?;
        let built = me.parent()?.parent()?.join("cactup");
        built.is_file().then_some(built)
    }

    /// A program run isolated is the second process of a PID namespace of
    /// its own, as this user; and the init refuses to be anything but the
    /// first process of one (else it would start itself again, and again).
    #[test]
    fn runs_a_program_in_a_namespace_of_its_own() {
        let Some(me) = cactup() else {
            eprintln!("skipped: no built cactup beside the tests");
            return;
        };
        if !isolation_by(&me) {
            eprintln!("skipped: no user namespaces here");
            return;
        }
        let sh = identity::find_program(OsStr::new("sh")).unwrap();
        let out = isolated_by(&me, &sh, OsStr::new("sh"), &["-c".into(), "echo $$ $(id -u)".into()]).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let said = String::from_utf8_lossy(&out.stdout);
        assert_eq!(said.trim(), format!("2 {}", rustix::process::getuid().as_raw()));
        // Its exit status passes through.
        let out = isolated_by(&me, &sh, OsStr::new("sh"), &["-c".into(), "exit 3".into()]).status().unwrap();
        assert_eq!(out.code(), Some(3));
        // Run plainly, outside any namespace: it stops at once.
        let out = Command::new(&me).arg(super::super::INIT_VERB).arg(&sh).arg("sh").arg("-c").arg("true").output().unwrap();
        assert_eq!(out.status.code(), Some(127));
        assert!(String::from_utf8_lossy(&out.stderr).contains("not the first process"));
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
        assert_eq!(shell_words(r#""/a b"/c -d 'e f' g\ h $X/y ${X}z $NO"#, get), ["/a b/c", "-d", "e f", "g h", "/x/y", "/xz", ""]);
        assert_eq!(shell_words("  ", get), Vec::<String>::new());
    }
}
