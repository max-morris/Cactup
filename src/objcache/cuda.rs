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
    if dry_run(path, name, &plain_compile(), None)? != run {
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
pub(super) fn relocates(path: &Path, name: &OsStr, dir: &Path) -> bool {
    let Ok(trial) = tempfile::Builder::new().prefix("cuda-trial-").tempdir_in(dir) else { return false };
    let source = "__device__ int twice(int v) { return 2 * v; }\n__global__ void k(int *p) { p[threadIdx.x] = twice(p[threadIdx.x]); }\nvoid launch(int *p) { k<<<1, 32>>>(p); }\n";
    let mut objects = Vec::new();
    for place in ["a", "b"] {
        let root = trial.path().join(place);
        if fs::create_dir_all(root.join("src")).is_err() || fs::write(root.join("src/trial.cu"), source).is_err() {
            return false;
        }
        let map = [(root.clone(), "/cactup-root")];
        let mut command = Command::new(path);
        command
            .arg0(name)
            .args(["-x", "cu", "-g", "-O2", "--relocatable-device-code=true", "-c", "-o"])
            .arg(root.join("trial.o"))
            .arg(root.join("src/trial.cu"))
            .args(map_flags(&root.join("src/trial.cu"), &map))
            .current_dir(&root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if !command.status().is_ok_and(|status| status.success()) {
            return false;
        }
        match fs::read(root.join("trial.o")) {
            Ok(object) => objects.push(object),
            Err(_) => return false,
        }
    }
    objects[0] == objects[1]
}

/// The flags that have an `nvcc` compile of `source` honor the path map
/// `map` (each directory and the name it is mapped to): the host
/// compiler's prefix map (through `-Xcompiler`), and the name `cudafe++`
/// is given for the source, of which the module ID is a hash.
pub fn map_flags(source: &Path, map: &[(PathBuf, &str)]) -> Vec<OsString> {
    let mut mapped = source.as_os_str().as_bytes().to_vec();
    let mut flags = Vec::new();
    for (from, to) in map {
        let mut from_slash = from.as_os_str().as_bytes().to_vec();
        if !from_slash.ends_with(b"/") {
            from_slash.push(b'/');
        }
        if let Some(rest) = mapped.strip_prefix(from_slash.as_slice()) {
            mapped = [to.as_bytes(), b"/", rest].concat();
        }
        let mut flag = OsString::from("-ffile-prefix-map=");
        flag.push(from.as_os_str());
        flag.push("=");
        flag.push(to);
        flags.push(OsString::from("-Xcompiler"));
        flags.push(flag);
    }
    flags.push(OsString::from("-Xcudafe"));
    let mut name = OsString::from("--orig_src_path_name=");
    name.push(OsStr::from_bytes(&mapped));
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

    #[test]
    fn splits_words_as_the_shell_does() {
        let get = |name: &str| (name == "X").then(|| "/x".to_owned());
        assert_eq!(shell_words(r#""/a b"/c -d 'e f' g\ h $X/y ${X}z $NO"#, get), ["/a b/c", "-d", "e f", "g h", "/x/y", "/xz", ""]);
        assert_eq!(shell_words("  ", get), Vec::<String>::new());
    }
}
