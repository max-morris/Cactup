//! The key of one compile: a digest that two compiles share exactly when
//! they would produce the same object (§18.1 rule 1).
//!
//! Two things are keyed for what the compiler reads, because neither alone
//! is what it compiles:
//!
//! - **The preprocessed text**: the output of the same compiler with the
//!   same arguments and `-E`. It shows what the include paths and macro
//!   definitions made of the source — which files were found, which
//!   branches of which conditionals were taken — so nothing about headers
//!   has to be guessed.
//! - **The bytes of every file that text was made from**, the source and
//!   each header the preprocessor names in its line markers. The text
//!   alone forgets what the compiler does not: spacing and comments move
//!   the columns that debug information and `__builtin_COLUMN` record, and
//!   Clang's debug information carries a checksum of each file.
//!
//! Around them go the things neither can show: which compiler it is, the
//! arguments that decide code generation, the platform, and the part of
//! the environment compilers read.
//!
//! **Paths.** Cactus compiles with absolute paths, and an object records
//! them (`__FILE__` in every `CCTK_WARN`, file names and the compile
//! directory in debug information), so as it stands an object belongs to
//! one configuration of one installation. Where it is known to be sound,
//! the key is computed as if the compile ran with the Cactus root and the
//! configuration directory mapped to fixed names ([`PathMap`]) — which is
//! how a serving cache will run it. Nothing is added to the real compile
//! while the cache only records.

use super::compile::{self, Compile};
use super::{cuda, fortran};
use super::hash::{bytes_digest, Hasher};
use super::identity::{self, Compiler, Family};
use super::{environment, platform, BuildConf};
use crate::Res;
use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, ErrorKind, Read as _};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};

/// What the Cactus root is called in a key, and in a mapped compile's
/// `__FILE__`: sources then read `/cactup-root/arrangements/<Arrangement>/<Thorn>/src/…`.
/// Absolute (decision 8): debug information then names every file by an
/// absolute path, which one `set substitute-path /cactup-root <tree>` in gdb
/// turns into the real one; a relative name would be taken as relative to
/// the recorded compile directory.
pub(super) const ROOT_NAME: &str = "/cactup-root/";
/// What the configuration directory is called, whatever its name.
pub(super) const CONFIG_NAME: &str = "/cactup-root/configs/@config/";

/// What stands for the configuration directory and the Cactus root (each
/// with its `/`) in the compiler messages a relocatable entry keeps.
const CONFIG_TOKEN: &[u8] = b"@CACTUP_CONFIG@/";
const ROOT_TOKEN: &[u8] = b"@CACTUP_ROOT@/";

/// Stored compiler messages as this build shows them: the tokens of
/// [`PathMap::messages_for_the_store`] as this build's directories.
pub fn messages_for_this_build(conf: &BuildConf, text: &[u8]) -> Vec<u8> {
    let dir = |path: &Path| [path.as_os_str().as_bytes(), b"/"].concat();
    let (config, root) = (dir(&conf.config_dir), dir(&conf.cactus_root));
    let mut out = Vec::with_capacity(text.len());
    let mut at = 0;
    while at < text.len() {
        if text[at..].starts_with(CONFIG_TOKEN) {
            out.extend_from_slice(&config);
            at += CONFIG_TOKEN.len();
        } else if text[at..].starts_with(ROOT_TOKEN) {
            out.extend_from_slice(&root);
            at += ROOT_TOKEN.len();
        } else {
            out.push(text[at]);
            at += 1;
        }
    }
    out
}

/// The pid of a preprocessor run in progress (0: none), for the wrapper's
/// signal handler: a stop signal that arrives while the key is checked
/// again after the compile must end that run too, not wait for it.
pub static PREPROCESSOR: AtomicI32 = AtomicI32::new(0);

/// The directories a key must not depend on, with the names that stand for
/// them.
///
/// The compiler applies a prefix map to whole file names, as plain string
/// prefixes; this map does the same to the same names, with the same
/// result. Each directory is mapped with its trailing `/`, so that
/// `/w/Cactus/` is not found at the start of `/w/Cactus-libs/include`.
#[derive(Debug)]
pub struct PathMap {
    /// Longest first: the configuration directory lies inside the Cactus
    /// root, and must be recognized before it.
    from_to: Vec<(Vec<u8>, &'static str)>,
}

impl PathMap {
    /// The map for a compile, if it is known to hold for it: the compiler
    /// was tried and made one object of the same sources in two places
    /// (`Compiler::relocates`), and the compile uses nothing known to put
    /// unmapped paths into the object (Clang's OpenMP source locations).
    ///
    /// Both the spelling cactup has for each directory and its physical
    /// path are mapped: make reports one, a shell's `pwd` may report the
    /// other.
    pub fn new(conf: &BuildConf, compiler: &Compiler, compile: &Compile) -> Option<Self> {
        let unmapped_paths = compiler.family == Family::Clang && compile.openmp;
        if !conf.relocate || !compiler.relocates || unmapped_paths {
            return None;
        }
        let mut from_to = Vec::new();
        for (dir, name) in [(&conf.config_dir, CONFIG_NAME), (&conf.cactus_root, ROOT_NAME)] {
            for spelling in [Some(dir.clone()), std::fs::canonicalize(dir).ok()].into_iter().flatten() {
                let mut spelling = spelling.into_os_string().into_vec();
                spelling.push(b'/');
                if !from_to.iter().any(|(from, _)| *from == spelling) {
                    from_to.push((spelling, name));
                }
            }
        }
        from_to.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
        Some(Self { from_to })
    }

    /// The compiler flags that make a compile honor the map, for `__FILE__`
    /// and for debug information. Shortest directory first, so that the
    /// last map that matches a name is the most specific one.
    pub fn flags(&self) -> Vec<OsString> {
        self.from_to.iter().rev().map(|(from, to)| map_flag(from, to)).collect()
    }

    /// What the map does to a compile, without where it is: the option
    /// and the names directories are mapped to. Part of every key made
    /// with the map, so that a cactup that maps another way keys apart.
    pub fn description() -> [&'static str; 3] {
        ["-ffile-prefix-map", ROOT_NAME, CONFIG_NAME]
    }

    /// A compiler's messages as an entry keeps them (§18.8): each directory
    /// the map knows, wherever it stands, as a token that
    /// [`messages_for_this_build`] turns back into this build's directory.
    pub fn messages_for_the_store(&self, text: &[u8]) -> Vec<u8> {
        let token = |to: &str| if to == CONFIG_NAME { CONFIG_TOKEN.to_vec() } else { ROOT_TOKEN.to_vec() };
        Rewrite::new(self.from_to.iter().map(|(from, to)| (from.clone(), token(to))).collect()).apply(text)
    }

    /// The map for [`identity`]'s trials of a compiler, in a tree at `root`
    /// with its configuration directory `config`: the map a key is made
    /// with, spelled one way.
    pub(super) fn for_trial(root: &Path, config: &Path) -> Self {
        let dir = |path: &Path| [path.as_os_str().as_bytes(), b"/"].concat();
        let mut from_to = vec![(dir(config), CONFIG_NAME), (dir(root), ROOT_NAME)];
        from_to.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
        Self { from_to }
    }

    /// `text` (a word of a command line, a value of the environment) with
    /// each directory the map knows replaced where a name begins: at the
    /// start, after a character no name has, or after a short option's
    /// letters (`-I/w/Cactus/…`, also quoted, as `INCLUDES` has them).
    pub fn in_text(&self, text: &[u8]) -> Vec<u8> {
        let in_a_path = |byte: u8| byte.is_ascii_alphanumeric() || b"._-+~@/".contains(&byte);
        // A short option's letters, from where a name could begin.
        let after_option = |at: usize| {
            let start = text[..at].iter().rposition(|byte| !in_a_path(*byte)).map_or(0, |before| before + 1);
            text[start] == b'-' && at > start + 1 && text[start + 1..at].iter().all(u8::is_ascii_alphabetic)
        };
        let mut out = Vec::with_capacity(text.len());
        let mut at = 0;
        while at < text.len() {
            let begins = at == 0 || !in_a_path(text[at - 1]) || after_option(at);
            match self.from_to.iter().find(|(from, _)| begins && text[at..].starts_with(from)) {
                Some((from, to)) => {
                    out.extend_from_slice(to.as_bytes());
                    at += from.len();
                }
                None => {
                    out.push(text[at]);
                    at += 1;
                }
            }
        }
        out
    }

    /// The file name `name` as a mapped compile records it.
    pub fn apply(&self, name: &[u8]) -> Vec<u8> {
        match self.from_to.iter().find_map(|(from, to)| Some((to, name.strip_prefix(from.as_slice())?))) {
            Some((to, rest)) => [to.as_bytes(), rest].concat(),
            None => name.to_vec(),
        }
    }

}

/// Names in compiler messages, each replaced by another where a path begins:
/// `/x/w/Cactus/` is not `/w/Cactus/`. A colored diagnostic puts an escape
/// sequence (`ESC[01mESC[K`) right before a path, and that ends in a letter.
#[derive(Debug, Clone)]
pub struct Rewrite {
    /// Longest first, so that the most specific name is the one replaced.
    pairs: Vec<(Vec<u8>, Vec<u8>)>,
}

impl Rewrite {
    pub fn new(mut pairs: Vec<(Vec<u8>, Vec<u8>)>) -> Self {
        pairs.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
        Self { pairs }
    }

    pub fn apply(&self, text: &[u8]) -> Vec<u8> {
        let in_a_path = |byte: u8| byte.is_ascii_alphanumeric() || b"._-+~@/".contains(&byte);
        let after_escape = |at: usize| {
            let before = &text[..at];
            let Some((_, rest)) = before.split_last().filter(|(last, _)| last.is_ascii_alphabetic()) else { return false };
            let digits = rest.iter().rev().take_while(|b| b.is_ascii_digit() || **b == b';').count();
            rest[..rest.len() - digits].ends_with(b"\x1b[")
        };
        let mut out = Vec::with_capacity(text.len());
        let mut at = 0;
        while at < text.len() {
            let begins = at == 0 || !in_a_path(text[at - 1]) || after_escape(at);
            match self.pairs.iter().find(|(from, _)| begins && text[at..].starts_with(from)) {
                Some((from, to)) => {
                    out.extend_from_slice(to);
                    at += from.len();
                }
                None => {
                    out.push(text[at]);
                    at += 1;
                }
            }
        }
        out
    }
}

/// The compiler flag that maps the directory `from` (with its trailing `/`)
/// to the name `to`.
fn map_flag(from: &[u8], to: &str) -> OsString {
    let mut flag = OsString::from("-ffile-prefix-map=");
    flag.push(OsStr::from_bytes(from));
    flag.push(format!("={to}"));
    flag
}

/// The flags of a [`PathMap`] for a Cactus tree at `root` and its
/// configuration directory `config`, for `identity::relocates` to try a
/// compiler with: built by the same code as the flags a key is made with.
pub fn trial_flags(root: &Path, config: &Path) -> Vec<OsString> {
    let dir = |path: &Path| [path.as_os_str().as_bytes(), b"/"].concat();
    vec![map_flag(&dir(root), ROOT_NAME), map_flag(&dir(config), CONFIG_NAME)]
}

/// The flags whose value is a file name that the compiler uses to find
/// files and records nowhere but in the names of what it finds — tried,
/// like the map itself, by the audit in `tests/objcache.rs`. Their values
/// are keyed as mapped. Every other argument is keyed as it stands: GCC
/// records its command line in debug information, unmapped, so a path of
/// the tree in `-frandom-seed=<path>` is part of the object.
const MAPPED_VALUES: &[&str] = &["-isystem", "-iquote", "-idirafter", "-include"];

/// The digests a key is made of. Kept apart in the event log, so that two
/// builds can be compared part by part: which part differed is why a
/// compile would have missed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Parts {
    pub platform: String,
    pub compiler: String,
    pub arguments: String,
    pub environment: String,
    /// The preprocessed text.
    pub text: String,
    /// The bytes of the files it was made from.
    pub files: String,
}

/// The label a key is made under. Objects keyed under one label are
/// served to every cactup build that uses it, and several builds share a
/// store at once (a queued job runs the build it was submitted with). So
/// **any change that makes one key stand for another object bumps it**:
/// something the key now covers that it did not (an input, a flag, a part
/// of the environment), anything cactup adds to or changes in a compile
/// it runs (the path map's flags are keyed by [`PathMap::description`],
/// but not every such change will be), or a change in how a part is
/// digested. A change that only narrows what is cached needs no bump.
pub const KEY_LABEL: &str = "key-19";

impl Parts {
    pub fn key(&self) -> String {
        let mut hasher = Hasher::new(KEY_LABEL);
        for part in [&self.platform, &self.compiler, &self.arguments, &self.environment, &self.text, &self.files] {
            hasher.feed(part.as_bytes());
        }
        hasher.hex()
    }
}

/// A compile the cache has a key for.
#[derive(Debug)]
pub struct Keyed {
    pub compile: Compile,
    pub compiler: Compiler,
    pub parts: Parts,
    /// How much preprocessed text, and how many files, went into the key.
    pub text_bytes: u64,
    pub files: usize,
    name: OsString,
    /// What the compiler's identity watches, checked again from memory.
    watch: identity::Watch,
    /// Right before the compile: did the identity still hold, and how did
    /// what it watches look ([`identity::Watch::stamps`])?
    before: Option<(bool, identity::Stamps)>,
    map: Option<PathMap>,
    /// What the files looked like when they were read for the key.
    seen: String,
    /// The dependency file the key's preprocessor run wrote, under a
    /// temporary name, and the name the compile would give it (§18.8).
    depend: Option<(tempfile::TempPath, PathBuf)>,
    /// A Fortran compile's copy, module files and dependency run (§18.10).
    fortran: Option<fortran::Fortran>,
    /// A CUDA compile's host compiler and preprocessor passes (M4).
    cuda: Option<cuda::Cuda>,
    /// What a CUDA compile runs, as keyed (decision 25).
    plan: Option<cuda::Plan>,
    /// Places a CUDA compile's last step looks in before it finds CUDA's
    /// own headers (`cuda::Plan::ahead`): all empty, and to stay so.
    ahead: Vec<PathBuf>,
    /// The files read, whole (`read_files`): what a hit whose messages are
    /// not empty must match besides the key (decision 29).
    whole: String,
    /// Where a C or C++ compile says what it read, to be held to what the
    /// key's run said it read (decision 28).
    own_reads: OwnReads,
    keyed_reads: BTreeSet<PathBuf>,
    /// The key leaves out definitions a unit never uses where it can (not
    /// with `build-cache-preserve-exact-warnings`, decision 31).
    narrow: bool,
}

/// Where a C or C++ compile says what it read (decision 28).
#[derive(Debug)]
enum OwnReads {
    /// Nowhere: Fortran and CUDA compiles are checked their own way.
    Nowhere,
    /// In a file cactup asks for (`-MD -MF`, added to the compile run to
    /// store its result), once it has asked.
    Ours(PathBuf, std::cell::Cell<bool>),
    /// In the dependency file the compile writes anyway (`-MD -MF`).
    Theirs(PathBuf),
}

/// Key the compile `argv` asks for (the compiler's words, then its
/// arguments). `Err` says why it is not one the cache keys; the compile
/// itself is none of this function's business.
///
/// `serving`: the compile may be served, and its result stored. A dependency
/// file the compile asks for is then written by the key's preprocessor run,
/// under a temporary name ([`Keyed::keep_depend`] gives it the compile's):
/// what a hit needs, since no compile runs; and a Fortran compile's copy is
/// written, which the compile for the store reads (§18.10).
///
/// The compiler's identity must hold from the start of the key to its end:
/// the key's own preprocessor runs use the compiler's programs, and a hit
/// is served on what they read (an assembler or compiler proper put where
/// they are searched for and gone again by the end is seen there).
pub fn key(conf: &BuildConf, cc_dir: &Path, argv: &[OsString], serving: bool) -> Result<Keyed, String> {
    let (keyed, identified) = key_made(conf, cc_dir, argv, serving)?;
    if !keyed.watch.holds() {
        return Err("the compiler's programs or libraries changed while the key was made".to_owned());
    }
    if !keyed.nothing_ahead() {
        return Err("a file appeared where nvcc's last step looks before CUDA's own headers while the key was made".to_owned());
    }
    if let Some(what) = identified.changed(&keyed.watch.stamps()) {
        return Err(format!("{what} changed while the key was made"));
    }
    Ok(keyed)
}

/// [`key`], and how what the compiler's identity watches looked right after
/// it was identified.
fn key_made(conf: &BuildConf, cc_dir: &Path, argv: &[OsString], serving: bool) -> Result<(Keyed, identity::Stamps), String> {
    let whole = |e: anyhow::Error| format!("{e:#}");
    // Before anything else: a variable that rules the compile out says so
    // whatever the compiler.
    environment::digest(true)?;
    // The compiler before its arguments: for a compiler the cache has no
    // reader for, "which compiler" is the reason worth giving, not whichever
    // of its flags the GCC reader trips over first.
    let fortran = compile::parse(&argv[1..]).is_ok_and(|compile| compile.language.is_fortran());
    if let Some(why) = driver_led_elsewhere(&argv[0], fortran, |variable| std::env::var_os(variable)) {
        return Err(why);
    }
    let (compiler, watch) = identity::identify_watching(cc_dir, &argv[0]).map_err(whole)?;
    let compile = match compiler.family {
        Family::Nvcc => compile::parse_cuda(&argv[1..])?,
        _ => compile::parse(&argv[1..])?,
    };
    // `nvcc`'s host compiler, for this compile: what it would run says
    // which (M4); its identity is watched with `nvcc`'s.
    // Its compiles are the same twice only with the module ID given
    // (decision 25).
    if compiler.family == Family::Nvcc && !compiler.takes_module_id {
        return Err("this nvcc does not take the module ID it is given, and the one it makes differs from compile to compile".to_owned());
    }
    // Flags `nvcc` takes from the environment, which no reader here sees.
    if compiler.family == Family::Nvcc
        && let Some(variable) = ["NVCC_PREPEND_FLAGS", "NVCC_APPEND_FLAGS"].into_iter().find(|variable| std::env::var_os(variable).is_some_and(|value| !value.is_empty()))
    {
        return Err(format!("{variable} adds flags to nvcc that the cache does not read"));
    }
    let (cuda_run, watch) = match compiler.family {
        Family::Nvcc => {
            let run = cuda::dry_run(&compiler.path, &argv[0], &argv[1..], None).map_err(whole)?;
            let (host, host_watch) = cuda::host(&run, cc_dir).map_err(whole)?;
            (Some((run, host)), watch.and(host_watch))
        }
        _ => (None, watch),
    };
    let identified = watch.stamps();
    if let Some(flag) = compile.keyed.iter().find(|argument| compiler.multilib.iter().any(|flag| argument.as_bytes() == flag.as_bytes())) {
        return Err(format!(
            "{} has the compiler use another of its sets of programs and libraries (a multilib), which the cache does not follow",
            flag.to_string_lossy()
        ));
    }
    if (compiler.family == Family::Gfortran) != compile.language.is_fortran() {
        return Err(match compile.language.is_fortran() {
            true => "Fortran is cached for gfortran only".to_owned(),
            false => "gfortran is cached for Fortran sources only".to_owned(),
        });
    }
    // The locale only where the compiler's trial says it can matter, and
    // wherever the compile converts character sets: the trial does not, and
    // a conversion such as `ASCII//TRANSLIT` follows the locale (§18.8).
    // With exact warnings (decision 31), the locale too: it says which
    // language they are printed in, and with which quotes.
    let environment = environment::digest(!compiler.locale_neutral || compile.charset || conf.exact_warnings)?;
    // A compile that converts character sets loads converters and locale
    // data nothing here follows (`GCONV_PATH`, `LOCPATH`, the system's
    // converter modules; tried: one charset name, two converters or two
    // locales, two objects). Cactus asks for none.
    if compile.charset {
        return Err("it converts character sets, with converters and locale data the cache does not follow".to_owned());
    }
    // For CUDA, what the host compiler does with them.
    let host_family = cuda_run.as_ref().map_or(compiler.family, |(_, host)| host.family);
    if host_family == Family::Clang && compile.forced_include {
        // Clang takes `<file>.pch` or `<file>.gch` in place of a file given
        // with `-include`, and its `-E` does not say so.
        return Err("-include with Clang may bring in a precompiled header, which the cache does not follow".to_owned());
    }
    let platform = platform::platform(conf, cc_dir).map_err(whole)?;
    if compile.native && platform.mixed {
        // Seen for real: one compile of one file, run twice, two objects.
        return Err("it targets the processor it runs on (native), and this host's processors are not all alike".to_owned());
    }
    // Both of a CUDA compile's compilers must make one object in two places;
    // and `nvcc` splits what it hands on (`-Xcompiler`, `-Xcudafe`) at
    // commas, so a map of a name with one cannot be given through it.
    let comma = |map: &PathMap| {
        let source = std::env::current_dir().map(|cwd| cwd.join(&compile.source)).unwrap_or_default();
        source.as_os_str().as_bytes().contains(&b',') || map.flags().iter().any(|flag| flag.as_bytes().contains(&b','))
    };
    // (A Clang host compiler's OpenMP source locations, as for Clang.)
    let unmapped_host = host_family == Family::Clang && compile.openmp;
    let map = PathMap::new(conf, &compiler, &compile).filter(|map| cuda_run.as_ref().is_none_or(|(_, host)| host.relocates && !unmapped_host && !comma(map)));
    let mapped = |text: &[u8]| map.as_ref().map_or_else(|| text.to_vec(), |map| map.apply(text));

    let mut arguments = Hasher::new("arguments");
    // `g++` compiles a `.c` file as C++: the driver's name decides with the
    // suffix, and it is in the compiler part; this is the suffix.
    arguments.feed(compile.language.name().as_bytes());
    match map {
        Some(_) => PathMap::description().iter().for_each(|word| arguments.feed(word.as_bytes())),
        None => arguments.feed(b"unmapped"),
    }
    let mut value_of_a_path_flag = false;
    for argument in &compile.keyed {
        match value_of_a_path_flag {
            true => arguments.feed(&mapped(argument.as_bytes())),
            false => arguments.feed(argument.as_bytes()),
        }
        value_of_a_path_flag = !value_of_a_path_flag && argument.to_str().is_some_and(|flag| MAPPED_VALUES.contains(&flag));
    }
    // Debug information records the directory the compiler ran in, under
    // the name the compiler has for it: the physical one, or `$PWD` when
    // that names the same place. Both go in, as a mapped compile would
    // record them (a directory is mapped as a file name is: the
    // configuration directory itself, having nothing after its name, is
    // not). Fortran finds module files from there by relative names, so
    // for Fortran it goes in always (§18.10); and so do its `-I`
    // directories, which say where the rest are found.
    if compile.language.is_fortran() {
        arguments.feed(b"fortran, its source named from the working directory");
        let mut flags = compile.preprocess.iter();
        while let Some(flag) = flags.next() {
            if flag == "-I" {
                arguments.feed(b"-I");
                arguments.feed(&mapped(flags.next().map(|dir| dir.as_bytes()).unwrap_or_default()));
            }
        }
    }
    // Exact warnings (decision 31) key whole files, which a narrowed key's
    // files can be byte for byte.
    if conf.exact_warnings {
        arguments.feed(b"exact warnings");
        // And how the compiler (for CUDA, its host compiler, whose messages
        // `nvcc` passes on) words a warning here: the language and quotes
        // follow the locale as this host resolves it (which locales and
        // message catalogs are installed), not only the variables keyed.
        let (path, fortran) = match &cuda_run {
            Some((_, host)) => (&host.path, false),
            None => (&compiler.path, compile.language.is_fortran()),
        };
        arguments.feed(&wording(path, fortran).map_err(whole)?);
    }
    // Whether each directory searched for headers is there: a compiler
    // warns of one that is not (gfortran always, GCC and Clang with
    // `-Wmissing-include-dirs`) or is no directory (GCC always), and with
    // `-Werror` fails.
    let mut words = compile.preprocess.iter().chain(&compile.keyed);
    while let Some(word) = words.next() {
        if ["-I", "-isystem", "-iquote", "-idirafter"].iter().any(|option| word == option)
            && let Some(dir) = words.next()
        {
            arguments.feed(b"include directory");
            arguments.feed(&mapped(dir.as_bytes()));
            arguments.feed(match std::fs::metadata(dir) {
                Ok(meta) if meta.is_dir() => b"there",
                Ok(_) => b"no directory",
                Err(_) => b"not there",
            });
        }
    }
    // A CUDA object records where it was compiled with or without debug
    // information, and the names of `nvcc`'s files beside the object, made
    // from the object's own (`--objdir-as-tempdir`; seen on qbd).
    let cuda = compile.language == compile::Language::Cuda;
    if compile.debug || compile.language.is_fortran() || cuda {
        let cwd = std::env::current_dir().context("Failed to read the working directory").map_err(whole)?;
        arguments.feed(b"cwd");
        arguments.feed(&mapped(cwd.as_os_str().as_bytes()));
        arguments.feed(&mapped(std::env::var_os("PWD").unwrap_or_default().as_bytes()));
        if cuda {
            arguments.feed(b"object");
            arguments.feed(&mapped(cwd.join(&compile.output).as_os_str().as_bytes()));
        }
    }
    // A CUDA compile runs as `nvcc` would run it, step by step (decision
    // 25): its steps, with the map's flags, each program the file its
    // identities found; and the key has them, mapped. Flags `nvcc` takes
    // from the environment through its profile (`PTXAS_FLAGS`,
    // `CUDAFE_FLAGS`) are in them.
    let mut ahead = Vec::new();
    let plan = match &cuda_run {
        Some((_, host)) => {
            let cwd = std::env::current_dir().context("Failed to read the working directory").map_err(whole)?;
            let mut args = argv[1..].to_vec();
            if let Some(map) = &map {
                args.extend(cuda::map_flags(&cwd.join(&compile.source), map));
            }
            let mut plan = cuda::Plan::new(&compiler.path, &argv[0], &args).map_err(whole)?;
            plan.resolve(host, &compiler.toolchain).map_err(whole)?;
            // What the last step finds by searching, which no pass reads,
            // must be CUDA's own; the places it looks in first must stay
            // empty to the end of the compile (looked at by name, decision
            // 20).
            ahead = plan.ahead(&compiler.host_reads).map_err(whole)?;
            arguments.feed(b"nvcc's steps");
            arguments.feed(&plan.described(|text| map.as_ref().map_or_else(|| text.to_vec(), |map| map.in_text(text))));
            Some(plan)
        }
        None => None,
    };

    let name = argv[0].clone();
    if compile.language.is_fortran() {
        let cwd = std::env::current_dir().context("Failed to read the working directory").map_err(whole)?;
        let read = fortran::key(&compiler, &name, &compile, &argv[1..], &cwd, map.as_ref(), cc_dir, serving).map_err(whole)?;
        let parts = Parts {
            platform: platform.digest,
            compiler: compiler.id.clone(),
            arguments: arguments.hex(),
            environment,
            text: read.text,
            files: read.files,
        };
        // (Fortran's files are keyed whole.)
        let files_whole = parts.files.clone();
        return Ok((Keyed {
            compile,
            compiler,
            parts,
            text_bytes: read.text_bytes,
            files: read.count,
            name,
            watch,
            before: None,
            map,
            seen: read.seen,
            depend: None,
            fortran: Some(read.fortran),
            cuda: None,
            plan: None,
            ahead: Vec::new(),
            whole: files_whole,
            own_reads: OwnReads::Nowhere,
            keyed_reads: BTreeSet::new(),
            narrow: false,
        }, identified));
    }
    if let Some((run, host)) = cuda_run {
        let read = cuda::key(&run, host, &compile, map.as_ref(), !conf.exact_warnings).map_err(whole)?;
        let mut both = Hasher::new("nvcc and its host compiler");
        both.feed(compiler.id.as_bytes());
        both.feed(read.cuda.host.id.as_bytes());
        let parts = Parts {
            platform: platform.digest,
            compiler: both.hex(),
            arguments: arguments.hex(),
            environment,
            text: read.text,
            files: read.files,
        };
        let plan = plan.map(|mut plan| {
            plan.set_id(cuda::module_id(&parts.key(), &compile.source));
            plan
        });
        let keyed = Keyed {
            compile,
            compiler,
            parts,
            text_bytes: read.text_bytes,
            files: read.count,
            name,
            watch,
            before: None,
            map,
            seen: read.seen,
            depend: None,
            fortran: None,
            cuda: Some(read.cuda),
            plan,
            ahead,
            whole: read.whole,
            own_reads: OwnReads::Nowhere,
            keyed_reads: BTreeSet::new(),
            narrow: !conf.exact_warnings,
        };
        return Ok((keyed, identified));
    }
    let depend = match serving {
        true => depend_flags(&compile).map_err(whole)?,
        false => None,
    };
    // What the compile reads must be what this run reads (decision 28): it
    // says so in its own dependency file where it writes one listing every
    // file (not `-MMD`'s, which leaves out system headers), else in one
    // cactup asks for; and this run says so alike, as the same compiler
    // words it (Clang lists what `__has_include` found, GCC does not).
    let own_reads = match (serving, compile.depend.is_empty()) {
        (false, _) => OwnReads::Nowhere,
        (true, true) => OwnReads::Ours(reads_file(&compile.output), std::cell::Cell::new(false)),
        (true, false) => {
            let full = compile.depend.iter().any(|flag| flag == "-MD") && !compile.depend.iter().any(|flag| flag == "-MMD");
            let file = compile.depend.iter().rposition(|flag| flag == "-MF").and_then(|at| compile.depend.get(at + 1));
            match (full, file) {
                (true, Some(file)) => OwnReads::Theirs(PathBuf::from(file)),
                _ => return Err("its dependency flags do not list every file it reads (-MMD leaves out system headers; without -MF the file has no name of its own)".to_owned()),
            }
        }
    };
    // This run's list: beside the dependency file it writes for the
    // compile, or in one of its own.
    let ours = match own_reads {
        OwnReads::Ours(..) => Some(tempfile::Builder::new().prefix(".cactup-").suffix(".d").tempfile().map_err(|e| format!("cannot make a file for the preprocessor's list of what it reads: {e}"))?.into_temp_path()),
        _ => None,
    };
    let mut flags: Vec<OsString> = depend.as_ref().map(|(_, flags)| flags.clone()).unwrap_or_default();
    if let Some(ours) = &ours {
        flags.extend(["-MD", "-MF"].map(OsString::from));
        flags.push(ours.to_path_buf().into_os_string());
    }
    // (Unless every warning must be the compile's own: decision 31.)
    let narrow = !conf.exact_warnings;
    let read = preprocess(&compiler, &name, &compile, map.as_ref(), (!flags.is_empty()).then_some(flags.as_slice()), narrow).map_err(whole)?;
    let listed = match (&ours, &depend) {
        (Some(ours), _) => Some(ours.to_path_buf()),
        (None, Some(((temp, _), _))) => Some(temp.to_path_buf()),
        _ => None,
    };
    let keyed_reads = match listed {
        Some(listed) => std::fs::read(&listed)
            .ok()
            .as_deref()
            .and_then(depend_files)
            .ok_or("the preprocessor did not say what it read")?
            .into_iter()
            .collect(),
        None => BTreeSet::new(),
    };
    drop(ours);
    let depend = depend.map(|(files, _)| files);
    let parts = Parts {
        platform: platform.digest,
        compiler: compiler.id.clone(),
        arguments: arguments.hex(),
        environment,
        text: read.text,
        files: read.files,
    };
    let keyed = Keyed {
        compile,
        compiler,
        parts,
        text_bytes: read.text_bytes,
        files: read.count,
        name,
        watch,
        before: None,
        map,
        seen: read.seen,
        depend,
        fortran: None,
        cuda: None,
        plan: None,
        ahead: Vec::new(),
        whole: read.whole,
        own_reads,
        keyed_reads,
        narrow,
    };
    Ok((keyed, identified))
}

/// The dependency flags of `compile` for the key's preprocessor run, if it
/// has any: `-MF` naming a temporary file beside the compile's, and the
/// target the compile would name when it is given none (`-MQ <object>`, as
/// the GCC and Clang drivers do for a compile with `-o`). With the
/// temporary file and the compile's name for it.
fn depend_flags(compile: &Compile) -> Res<Option<((tempfile::TempPath, PathBuf), Vec<OsString>)>> {
    if compile.depend.is_empty() {
        return Ok(None);
    }
    // Flags and their values, as `compile::parse` lists them: `-MF`, `-MT`
    // and `-MQ` with the value after (which may itself look like a flag),
    // the others alone.
    let mut flags_at = Vec::new();
    let mut at = 0;
    while at < compile.depend.len() {
        flags_at.push(at);
        at += if ["-MF", "-MT", "-MQ"].iter().any(|flag| compile.depend[at] == *flag) { 2 } else { 1 };
    }
    let named = |flag: &str| flags_at.iter().any(|at| compile.depend[*at] == flag);
    // The last `-MF` is the one the compiler writes.
    let at = *flags_at.iter().rev().find(|at| compile.depend[**at] == "-MF").context("a dependency file without -MF")?;
    let real = PathBuf::from(&compile.depend[at + 1]);
    let dir = real.parent().filter(|dir| !dir.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = real.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    // Created as the compiler creates the file (`0666`, less the umask, and
    // what a default ACL adds): the preprocessor writes into it in place.
    let temp = tempfile::Builder::new()
        .prefix(&format!(".{name}.cactup-"))
        .permissions(std::fs::Permissions::from_mode(0o666))
        .tempfile_in(dir)
        .with_context(|| format!("Failed to create a temporary file in {}", dir.display()))?
        .into_temp_path();
    let mut flags = compile.depend.clone();
    flags[at + 1] = temp.as_os_str().to_owned();
    if !named("-MT") && !named("-MQ") {
        flags.push(OsString::from("-MQ"));
        flags.push(compile.output.clone().into_os_string());
    }
    Ok(Some(((temp, real), flags)))
}

impl Keyed {
    /// Is the key free of where the installation is and what the
    /// configuration is called?
    pub fn relocatable(&self) -> bool {
        self.map.is_some()
    }

    /// Are the preprocessed text and the files behind it still what was
    /// keyed? Run after the compile: a header edited while the compile ran
    /// makes an object of the new text under the key of the old.
    ///
    /// Each file must also look as it did when it was keyed: a file changed
    /// and changed back during the compile has its old bytes, and the
    /// object may still have the new ones (§18.5).
    ///
    /// For Fortran, where [`Keyed::before_compile`] worked out where the
    /// compile looks, the files are read again and those places looked at
    /// again; otherwise the dependency run runs again (§18.10).
    pub fn still_holds(&mut self) -> bool {
        // The compiler too: an assembler that appeared on `PATH` while the
        // compile ran assembled it (tried). What its identity watches must
        // hold, as right before the compile, with nothing come or gone.
        match &self.before {
            Some((true, stamps)) if self.watch.holds() && *stamps == self.watch.stamps() && self.nothing_ahead() => {}
            _ => return false,
        }
        if let Some(fortran) = &mut self.fortran {
            return fortran::still_holds(&self.compiler, &self.name, self.map.as_ref(), fortran, &self.parts.files, &self.seen);
        }
        if let (Some(cuda), Some(plan)) = (&self.cuda, &self.plan) {
            return cuda::still_holds(cuda, plan, &self.compiler.host_reads, &self.compile, self.map.as_ref(), &self.parts, &self.seen);
        }
        // What the compile says it read (removed if cactup asked for it),
        // where it said it.
        let said_read = match &self.own_reads {
            OwnReads::Ours(file, asked) if asked.get() => {
                let said = std::fs::read(file);
                let _ = std::fs::remove_file(file);
                match said.ok().as_deref().and_then(depend_files) {
                    Some(read) => Some(read),
                    None => return false,
                }
            }
            OwnReads::Theirs(file) => match std::fs::read(file).ok().as_deref().and_then(depend_files) {
                Some(read) => Some(read),
                None => return false,
            },
            _ => None,
        };
        preprocess(&self.compiler, &self.name, &self.compile, self.map.as_ref(), None, self.narrow).is_ok_and(|read| {
            read.text == self.parts.text
                && read.files == self.parts.files
                && read.seen == self.seen
                && said_read.is_none_or(|said| reads_match(&said, &self.keyed_reads))
        })
    }

    /// Get ready for the check after the compile, right before the compile:
    /// for Fortran, work out the places the compile passes over, so that the
    /// check can look at them instead of running the dependency run again
    /// (§18.10). C and C++ are checked by a second preprocessor run.
    pub fn before_compile(&mut self) {
        // The compiler's identity, as for the key: what appeared since then
        // where its programs are searched for would run (tried: put there
        // during the key's preprocessor run, gone by the check after).
        self.before = Some((self.watch.holds(), self.watch.stamps()));
        if let Some(fortran) = &mut self.fortran {
            fortran.before_compile(&self.compiler, &self.name);
        }
    }

    /// For a CUDA compile, what runs in place of the compile: `nvcc`'s own
    /// steps, as keyed, with the module ID made of the key (decision 25).
    pub fn plan(&self) -> Option<&cuda::Plan> {
        self.plan.as_ref()
    }

    /// The files read, whole: what an entry keeps besides the key, for a hit
    /// whose messages speak of definitions (decisions 29, 30).
    pub fn whole(&self) -> &str {
        &self.whole
    }

    /// Does the key leave out anything of the files read (decision 26)?
    pub fn narrowed(&self) -> bool {
        self.whole != self.parts.files
    }

    /// Is every place a CUDA compile's last step looks in before CUDA's own
    /// headers still empty?
    fn nothing_ahead(&self) -> bool {
        self.ahead.iter().all(|place| std::fs::symlink_metadata(place).is_err())
    }

    /// How the check after a Fortran compile was made: by lookups in place
    /// of the dependency run (how many), or why it ran that again. `None`
    /// for C and C++, which always run the preprocessor again.
    pub fn check_made(&self) -> Option<Result<u64, String>> {
        self.fortran.as_ref().map(fortran::Fortran::check_made)
    }

    /// The command line of the compile whose result is to be stored, in
    /// place of `argv`: the recipe's, given the flags that make it record its
    /// paths as the key does (the path map's, if the key was made with it),
    /// and for Fortran under the map, the source named as the map names it
    /// (§18.10).
    pub fn compile_argv(&self, argv: &[OsString]) -> Vec<OsString> {
        let mut out = vec![argv[0].clone()];
        match &self.fortran {
            Some(fortran) => out.extend(fortran.arguments(&argv[1..], self.compile.source_at)),
            None => out.extend(argv[1..].iter().cloned()),
        }
        match &self.cuda {
            // `nvcc`'s own form of them, and the name `cudafe++` makes the
            // module ID from (M4), mapped where the map holds.
            Some(_) => {
                if let Some(map) = &self.map {
                    let source = std::env::current_dir().map(|cwd| cwd.join(&self.compile.source)).unwrap_or_else(|_| self.compile.source.clone());
                    out.extend(cuda::map_flags(&source, map));
                }
            }
            None => {
                out.extend(self.map.as_ref().map(PathMap::flags).unwrap_or_default());
                // The compile says what it reads (decision 28).
                if let OwnReads::Ours(file, asked) = &self.own_reads {
                    out.extend(["-MD", "-MF"].map(OsString::from));
                    out.push(file.clone().into_os_string());
                    asked.set(true);
                }
            }
        }
        out
    }

    /// The module files the compile writes into its working directory
    /// (§18.10): none, but for Fortran.
    pub fn modules(&self) -> &[String] {
        self.fortran.as_ref().map_or(&[], |fortran| fortran.modules.as_slice())
    }

    /// What the compile's messages say in place of real names, when it reads
    /// a renamed copy (§18.10).
    pub fn rewrite(&self, conf: &BuildConf) -> Option<Rewrite> {
        self.fortran.as_ref()?.renamed.as_ref().map(|renamed| renamed.rewrite(conf))
    }

    /// Remove what a compile that failed was asked to say it read
    /// (decision 28): nothing checks it.
    pub fn forget_reads(&self) {
        if let OwnReads::Ours(file, _) = &self.own_reads {
            let _ = std::fs::remove_file(file);
        }
        if let Some(plan) = &self.plan {
            plan.tidy_from(plan.steps().len());
        }
    }

    /// Remove what keying made: the copy and the dependency runs' module
    /// files. (The process ends by `exit`, which runs no destructor.)
    pub fn clean_up(&mut self) {
        drop(self.fortran.take());
        if let OwnReads::Ours(file, _) = &self.own_reads {
            let _ = std::fs::remove_file(file);
        }
        if let Some(plan) = &self.plan {
            plan.tidy_from(plan.steps().len());
        }
    }

    /// The map the key was made with.
    pub fn map(&self) -> Option<&PathMap> {
        self.map.as_ref()
    }

    /// The dependency file the key's preprocessor run wrote, and the name
    /// the compile gives its own: what audit mode compares.
    pub fn depend_files(&self) -> Option<(&Path, &Path)> {
        self.depend.as_ref().map(|(temp, real)| (temp.as_ref(), real.as_path()))
    }

    /// Remove the dependency file the key's preprocessor run wrote: the
    /// compile writes its own. (The process ends by `exit`, which runs no
    /// destructor: this has to be asked for.)
    pub fn drop_depend(&mut self) {
        drop(self.depend.take());
    }

    /// Give the dependency file the key's preprocessor run wrote the name
    /// the compile would have given it. Nothing, if none was asked for.
    pub fn keep_depend(&mut self) -> std::io::Result<()> {
        match self.depend.take() {
            Some((temp, real)) => temp.persist(&real).map_err(|e| e.error),
            None => Ok(()),
        }
    }
}

/// What the preprocessor read for one compile.
struct Read {
    /// Digest of its output.
    text: String,
    text_bytes: u64,
    /// Digest of the contents of the files it named, and of them whole
    /// ([`read_files`]).
    files: String,
    whole: String,
    count: usize,
    /// Digest of what each of those files looked like when it was read
    /// (device, inode, size, modification and change time, of the name and
    /// of what it leads to): not part of the key, but of the check after the
    /// compile. A file changed and changed back in between has its old
    /// bytes again, but not its old change time, which nothing can set back.
    seen: String,
}

/// A line that names a file: a line marker of preprocessor output
/// (`# 12 "dir/file.h" 1`), or a line directive of a source (`#line 12
/// "file.c"`).
pub(super) struct Naming<'a> {
    /// What stands before the file name, the opening quote included.
    pub head: &'a [u8],
    /// The name, with its escapes undone.
    pub name: Vec<u8>,
    /// What follows the name, from the closing quote on.
    pub tail: &'a [u8],
}

impl Naming<'_> {
    /// Does the marker say the preprocessor *entered* the file (flag `1`)?
    /// Then it opened it. A marker without the flag returns to a file, or
    /// repeats what a `#line` in the source said.
    fn enters(&self) -> bool {
        self.tail[1..].split(|b| b.is_ascii_whitespace()).any(|flag| flag == b"1")
    }
}

/// Take apart a line that begins with `opening` (`# ` in preprocessor
/// output, `#line ` in a source), then a line number and a quoted file
/// name. `Ok(None)`: the line does not begin so. `Err`: it does, and the
/// name cannot be read — which must not pass for "names no file".
///
/// Compilers write a name as a C string: GCC and Clang escape `\\` and
/// `"`, and write bytes they take for unprintable (a tab, anything outside
/// ASCII) as octal.
pub(super) fn naming<'a>(line: &'a [u8], opening: &[u8]) -> Result<Option<Naming<'a>>, String> {
    let Some(rest) = line.strip_prefix(opening) else { return Ok(None) };
    let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
    let Some(quoted) = rest[digits..].strip_prefix(b" \"").filter(|_| digits > 0) else { return Ok(None) };
    let head = &line[..line.len() - quoted.len()];
    let unreadable = || format!("a file name in a line marker cannot be read: {}", String::from_utf8_lossy(line).trim_end());
    let (mut name, mut at) = (Vec::new(), 0);
    loop {
        let byte = *quoted.get(at).ok_or_else(unreadable)?;
        at += 1;
        match byte {
            b'"' => return Ok(Some(Naming { head, name, tail: &quoted[at - 1..] })),
            b'\\' => {
                let escape = *quoted.get(at).ok_or_else(unreadable)?;
                at += 1;
                name.push(match escape {
                    b'\\' | b'"' | b'\'' | b'?' => escape,
                    b'a' => 0x07,
                    b'b' => 0x08,
                    b'f' => 0x0c,
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    b'v' => 0x0b,
                    b'0'..=b'7' => {
                        // Up to three octal digits, the first already read.
                        let mut value = u32::from(escape - b'0');
                        for _ in 0..2 {
                            match quoted.get(at) {
                                Some(digit @ b'0'..=b'7') => value = value * 8 + u32::from(digit - b'0'),
                                _ => break,
                            }
                            at += 1;
                        }
                        u8::try_from(value).map_err(|_| unreadable())?
                    }
                    b'x' => {
                        let digits = quoted[at..].iter().take_while(|b| b.is_ascii_hexdigit()).count();
                        let text = std::str::from_utf8(&quoted[at..at + digits]).map_err(|_| unreadable())?;
                        at += digits;
                        u8::from_str_radix(text, 16).map_err(|_| unreadable())?
                    }
                    _ => return Err(unreadable()),
                });
            }
            byte => name.push(byte),
        }
    }
}

/// The bytes of the file at `path` as they go into a key.
///
/// They are the file's own bytes, with one exception. Cactus begins the
/// build copy of a source with a line directive that names the file it was
/// copied from, by its absolute path — which no two installations share.
/// The compiler takes a name from that line and nothing else, and maps the
/// name; so under a map the name is fed as mapped. Only in a first line:
/// further down, what looks like a directive may be the inside of a comment
/// or of a string.
fn content_digest(bytes: &[u8], map: Option<&PathMap>) -> String {
    let Some(map) = map else { return bytes_digest(bytes) };
    let first = bytes.iter().position(|b| *b == b'\n').map_or(bytes.len(), |at| at + 1);
    let mut hasher = Hasher::new("file");
    // A first line that is no such directive, or one whose name cannot be
    // read, is bytes like any other.
    match naming(&bytes[..first], b"#line ").ok().flatten() {
        Some(Naming { head, name, tail }) => {
            hasher.feed(b"line directive");
            hasher.feed(head);
            hasher.feed(&map.apply(&name));
            hasher.feed(tail);
            hasher.feed(&bytes[first..]);
        }
        None => hasher.feed(bytes),
    }
    hasher.hex()
}

/// One step of resolving a name: what the entry was (device, inode, and
/// what tells a new entry from an old one under a reused inode number:
/// its birth time, and a symlink's change time), and where a symlink led.
#[derive(Clone)]
pub(super) enum Step {
    Entry(Vec<u8>, Option<PathBuf>),
    Absent,
}

/// Feed `seen` with every entry the name `path` resolves through — each
/// directory and each symlink, by device and inode, and each symlink's
/// target — from the root (or from the working directory, itself resolved
/// so), following symlinks as the kernel does. A symlink swapped and
/// swapped back is a new symlink, and a directory replaced is another
/// directory: either shows, also where the filesystem gives the new one the
/// old one's inode number (ext4 does at once), by its birth time (where
/// the filesystem keeps one) and, for a symlink, its change time. Neither
/// moves when files are created inside a directory. (A directory renamed away and the same one renamed
/// back does not: decision 9, spec §18.5.) `steps` remembers each entry
/// looked at, for the many names that share their directories.
pub(super) fn trail(path: &Path, seen: &mut Hasher, steps: &mut HashMap<PathBuf, Step>) {
    let mut pending: std::collections::VecDeque<std::ffi::OsString> = std::collections::VecDeque::new();
    let start = match path.is_absolute() {
        true => path.to_owned(),
        false => std::env::current_dir().unwrap_or_default().join(path),
    };
    let push_front = |pending: &mut std::collections::VecDeque<OsString>, path: &Path| {
        for component in path.components().rev() {
            pending.push_front(component.as_os_str().to_owned());
        }
    };
    push_front(&mut pending, &start);
    let mut at = PathBuf::from("/");
    let mut links = 0;
    while let Some(component) = pending.pop_front() {
        match component.as_bytes() {
            b"/" => at = PathBuf::from("/"),
            b"." => {}
            b".." => {
                at.pop();
            }
            _ => {
                let next = at.join(&component);
                let step = steps
                    .entry(next.clone())
                    .or_insert_with(|| match std::fs::symlink_metadata(&next) {
                        Ok(meta) => {
                            let symlink = meta.file_type().is_symlink();
                            let mut what = Vec::new();
                            what.extend(meta.dev().to_le_bytes());
                            what.extend(meta.ino().to_le_bytes());
                            match meta.created().ok().and_then(|born| born.duration_since(std::time::UNIX_EPOCH).ok()) {
                                Some(born) => what.extend(born.as_nanos().to_le_bytes()),
                                None => what.extend(b"no birth time"),
                            }
                            if symlink {
                                what.extend(meta.ctime().to_le_bytes());
                                what.extend(meta.ctime_nsec().to_le_bytes());
                            }
                            let target = symlink.then(|| std::fs::read_link(&next).unwrap_or_default());
                            Step::Entry(what, target)
                        }
                        Err(_) => Step::Absent,
                    })
                    .clone();
                match step {
                    Step::Entry(what, target) => {
                        seen.feed(&what);
                        match target {
                            // A symlink: resolve its target in its place.
                            Some(target) => {
                                seen.feed(target.as_os_str().as_bytes());
                                links += 1;
                                if links > 40 {
                                    seen.feed(b"too many links");
                                    return;
                                }
                                push_front(&mut pending, &target);
                            }
                            None => at = next,
                        }
                    }
                    Step::Absent => {
                        seen.feed(b"absent");
                        return;
                    }
                }
            }
        }
    }
}

/// Does `line` of preprocessed text make the assembler read another file
/// (`.incbin "blob"`, `.include "more.s"`, usually inside an `asm`
/// string)? The object then holds bytes no key here covers.
fn assembler_include(line: &[u8]) -> bool {
    [b".incbin".as_slice(), b".include"].iter().any(|directive| {
        line.windows(directive.len()).enumerate().filter(|(_, window)| window == directive).any(|(at, _)| {
            let rest = &line[at + directive.len()..];
            matches!(rest.iter().find(|b| !matches!(b, b' ' | b'\t')), Some(b'"' | b'\\'))
        })
    })
}

/// The preprocessor run for `compile`: the compiler that was identified,
/// under the name the recipe ran it by, with the compile's arguments and
/// `-E` in place of `-c -o <object>` — and without the flags that would
/// have it write a dependency file (`Compile::depend`), which is the
/// compile's to write.
fn preprocessor(compiler: &Compiler, name: &OsStr, compile: &Compile, map: Option<&PathMap>, depend: Option<&[OsString]>) -> Command {
    let mut command = Command::new(&compiler.path);
    // `-v`: the driver then says, on stderr, where it takes flags from
    // besides its command line (see `flags_from_elsewhere`).
    command.arg0(name).args(&compile.preprocess).args(["-E", "-v"]);
    if let Some(depend) = depend {
        command.args(depend);
    }
    if compile.macros_in_debug {
        command.arg("-dD");
    } else if compiler.family == Family::Gcc {
        // The macros the text expands or tests, where it first does
        // (decision 26: the others' definitions are not keyed).
        command.arg("-dU");
    }
    if compiler.family == Family::Gcc {
        command.arg("-fpch-preprocess");
    }
    if let Some(map) = map {
        command.args(map.flags());
    }
    in_english(&mut command);
    command
}

/// Have `command`, a compiler driver, word its own messages in English
/// ([`english_messages`]).
pub fn in_english(command: &mut Command) {
    for (variable, value) in english_messages(std::env::var_os("LC_ALL")) {
        match value {
            Some(value) => command.env(variable, value),
            None => command.env_remove(variable),
        };
    }
}

/// Run the preprocessor for `compile` and digest what it prints and what it
/// read: the compiler that was identified, under the name the recipe ran it
/// by, with the compile's arguments and `-E` in place of `-c -o <object>`.
///
/// With `-g3`, macro definitions are kept in the text (`-dD`), since the
/// object's debug information then has them. GCC is also asked to say
/// where it would have used a precompiled header (`-fpch-preprocess`): it
/// uses one found beside a header unasked, and the header's text is then
/// not what is compiled.
///
/// The compiler maps `__FILE__` where the text uses it (it is given the
/// map's flags). It does not map the file names in its line markers, so
/// those are mapped here, the way the compiler maps names; and each named
/// file's bytes are digested under its mapped name.
fn preprocess(compiler: &Compiler, name: &OsStr, compile: &Compile, map: Option<&PathMap>, depend: Option<&[OsString]>, narrow: bool) -> Res<Read> {
    let mut command = preprocessor(compiler, name, compile, map, depend);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("Failed to run {} as a preprocessor", compiler.path.display()))?;
    PREPROCESSOR.store(child.id() as i32, Ordering::SeqCst);
    // Read aside, so that a preprocessor with much to say (warnings) does
    // not stop on a full pipe while its output is being read here.
    let said = child.stderr.take().map(|mut stderr| {
        std::thread::spawn(move || {
            let mut said = Vec::new();
            let _ = stderr.read_to_end(&mut said);
            said
        })
    });
    let read = digest_output(&mut child, map);
    let status = child.wait();
    PREPROCESSOR.store(0, Ordering::SeqCst);
    let said = said.and_then(|said| said.join().ok()).unwrap_or_default();
    let Output { text, bytes: text_bytes, mut named, used, with_code, front_end_pragma } = read?;
    let status = status.context("Failed to wait for the preprocessor")?;
    if !status.success() {
        bail!("the preprocessor failed ({status})");
    }
    flags_from_elsewhere(compiler.family, compiler.specs.as_deref(), &said).map_err(anyhow::Error::msg)?;
    let text = text_and_said(&text, &said_of_the_text(&said, map));
    // The source itself, whatever the markers call it.
    let source = compile.source.as_os_str().as_bytes();
    named.insert((map.map_or_else(|| source.to_vec(), |map| map.apply(source)), compile.source.clone()), true);

    // Narrowed where nothing the run said of the text names a file it
    // would narrow: a warning about a definition (a stray quote, `/*` in a
    // comment, a name redefined) is the compile's too, and would be about a
    // line the key leaves out; the files a diagnostic names are kept whole
    // (decision 30). Not narrowed where the run may not have said all the
    // compile will ([`said_as_compiled`]).
    let diagnosed = diagnosed_files(&said).filter(|_| said_as_compiled(compile.language, front_end_pragma));
    let used = match (narrow && compiler.family == Family::Gcc && !compile.macros_in_debug, diagnosed) {
        (true, Some(diagnosed)) => Some(Used::new(used, with_code, diagnosed, compile)),
        _ => None,
    };
    let (files, whole, seen) = read_files(&named, map, used.as_ref())?;
    Ok(Read { text, text_bytes, files, whole, count: named.len(), seen })
}

/// Digest the files `named`, each under its mapped name: their bytes (the
/// key's `files` part), and what each looked like when it was read, with
/// every entry its name resolves through (the check after the compile).
///
/// A file the compiler opened (`entered`): if that cannot be read here, its
/// name was misread or it is gone, and there is no key. A name that only a
/// `#line` gave (generated code names its origin so) may be of no file at
/// all; the bytes compiled are those of the file the directive stands in,
/// which was entered.
///
/// With `used` (GCC, without `-g3`: decision 26), the lines of a file but
/// the unit's own source that define or undefine a macro the text neither
/// expands nor tests are left out of its bytes ([`Used::droppable`]): such a
/// definition changes nothing the compiler makes of the unit. Cactus's
/// headers that list every thorn of a configuration (`cctk_DefineThorn.h`,
/// `CParameterStructNames.h`) are of that kind for nearly every unit. They
/// are left out whole, line breaks and all (decision 29): where what is
/// compiled stands is in the text (`-E` keeps its lines where they were),
/// and where a definition stands shows only in what the compiler says
/// ("in definition of macro" names its line). So the files part does not
/// have the definitions' places, and the second digest returned does:
/// every file's bytes whole, which an entry keeps beside its messages, and
/// a hit whose messages speak of definitions must match
/// ([`Store::restore_with`], decision 30).
///
/// [`Store::restore_with`]: super::store::Store::restore_with
pub(super) fn read_files(named: &Named, map: Option<&PathMap>, used: Option<&Used>) -> Res<(String, String, String)> {
    let mut files = Hasher::new("files");
    let mut whole = Hasher::new("files");
    let mut seen = Hasher::new("seen");
    let mut steps = HashMap::new();
    let mut reads = Vec::with_capacity(named.len());
    for ((mapped, path), entered) in named {
        // What the name resolves through, and the file it leads to as the
        // open file is (after the open, which on NFS revalidates what the
        // client knows of it), before its bytes are read: a change after
        // this shows in the check after the compile.
        seen.feed(path.as_os_str().as_bytes());
        trail(path, &mut seen, &mut steps);
        let read = std::fs::File::open(path).and_then(|mut file| {
            let meta = file.metadata()?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            Ok((meta, bytes))
        });
        match &read {
            Ok((meta, _)) => {
                for number in [meta.dev(), meta.ino(), meta.len()] {
                    seen.feed(&number.to_le_bytes());
                }
                for time in [meta.mtime(), meta.mtime_nsec(), meta.ctime(), meta.ctime_nsec()] {
                    seen.feed(&time.to_le_bytes());
                }
            }
            Err(_) => seen.feed(b"absent"),
        }
        // Its warning, and with `-Werror` its failure, depends on when files
        // were last changed; `-E` does not show it.
        if let Ok((_, bytes)) = &read
            && pragma_dependency(bytes)
        {
            bail!("#pragma GCC dependency compares when files were last changed, which the cache does not follow");
        }
        reads.push((mapped, path, *entered, read.map(|(_, bytes)| bytes)));
    }
    // How often each macro is defined or undefined across the unit's files:
    // one defined twice may draw a warning that it is redefined, which the
    // compile says, and so stays in.
    let mut defined: HashMap<&[u8], usize> = HashMap::new();
    if used.is_some() {
        for (_, _, _, read) in &reads {
            for name in read.iter().flat_map(|bytes| definitions(bytes)) {
                *defined.entry(name).or_default() += 1;
            }
        }
    }
    for (mapped, path, entered, read) in &reads {
        files.feed(mapped);
        whole.feed(mapped);
        // "Not there" is the one thing a made-up name may be: any other
        // failure to read is a failure to key.
        let absent = || matches!(path.metadata(), Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory));
        match read {
            Ok(bytes) => {
                let all = content_digest(bytes, map);
                whole.feed(all.as_bytes());
                match used {
                    Some(used) if !used.keeps(path) => {
                        let kept = without_definitions(bytes, |name| used.droppable(name) && defined.get(name) == Some(&1));
                        files.feed(content_digest(&kept, map).as_bytes());
                    }
                    _ => files.feed(all.as_bytes()),
                }
            }
            Err(_) if !*entered && absent() => {
                files.feed(b"no such file");
                whole.feed(b"no such file");
            }
            Err(e) => bail!("a file the compile reads cannot be read to key it (Failed to read {}: {e})", path.display()),
        }
    }
    Ok((files.hex(), whole.hex(), seen.hex()))
}

/// Does the driver take flags for this compile from a file of its own? It
/// says so itself when run with `-v`: `said` is what the preprocessor run
/// — the compile's own arguments and environment — wrote to stderr.
///
/// Such flags never pass the reader of the command line (`compile`), so
/// nothing it would decline is declined. Which file a driver reads depends
/// on the compile (Clang picks a configuration file by target, so `-m32`
/// can bring one in; an environment variable can turn them off), which is
/// why this is asked of every compile, and of the compiler as a whole only
/// to save the asking (`identity::examine`).
///
/// A GCC whose specs file was accepted for changing only the link
/// (`specs`) must say it read that one file, `specs`, and nothing else.
pub(super) fn flags_from_elsewhere(family: Family, specs: Option<&Path>, said: &[u8]) -> Result<(), String> {
    let said = String::from_utf8_lossy(said);
    // Silence is not a "no": each family has lines it always prints, and
    // an answer without them (lost, cut short, in another version's or
    // another language's wording) is no answer. Clang names a
    // configuration file after `InstalledDir:` and before the command line
    // of the compiler proper, so it has to have got as far as that.
    match family {
        Family::Clang => match said.lines().find_map(|line| line.strip_prefix("Configuration file: ")) {
            Some(file) => Err(format!("the compiler reads a configuration file ({file}), which can add flags the cache does not see")),
            None if said.lines().any(|line| line.starts_with("InstalledDir: ")) && said.lines().any(runs_cc1) => Ok(()),
            None => Err("the compiler does not say whether it reads a configuration file".to_owned()),
        },
        // `nvcc`'s preprocessor passes are its host compiler's, asked of that.
        Family::Nvcc => Err("nvcc is asked for the flags it takes from elsewhere by its host compiler".to_owned()),
        Family::Gcc | Family::Gfortran => {
            let read: Vec<&str> = said.lines().filter_map(|line| line.strip_prefix("Reading specs from ")).collect();
            let accepted = |file: &str| specs.is_some_and(|specs| std::fs::canonicalize(file).is_ok_and(|file| file == specs));
            match (read.as_slice(), specs) {
                ([], None) if said.lines().any(|line| line == "Using built-in specs.") => Ok(()),
                ([file], Some(_)) if accepted(file) => Ok(()),
                ([], _) => Err("the compiler does not say whether it reads a specs file".to_owned()),
                (files, _) => match files.iter().find(|file| !accepted(file)) {
                    Some(file) => Err(format!("the compiler reads a specs file ({file}), which can add flags the cache does not see")),
                    None => Err("the compiler reads its specs file more than once".to_owned()),
                },
            }
        }
    }
}

/// Is `line` of a Clang driver's `-v` output the command line of the
/// compiler proper (`"/usr/bin/clang" -cc1 -triple …`; older versions quote
/// every word), or the compiler proper's own first line (`clang -cc1
/// version …`, which is what is found when the driver's path has a space
/// in it)? Either comes after the place where a configuration file is
/// named.
fn runs_cc1(line: &str) -> bool {
    line.split_whitespace().nth(1).is_some_and(|word| word.trim_matches('"') == "-cc1")
}

/// The changes to the environment of a preprocessor run that make the
/// driver's own messages English (GCC translates "Using built-in specs."),
/// and nothing else: every other locale category stays what it was, since
/// a compiler may read its source by `LC_CTYPE`. `lc_all` is the value of
/// `LC_ALL`, which overrides every category and so has to be taken apart
/// into them (the ones a compiler could read; an empty value is no value,
/// as the C library has it).
fn english_messages(lc_all: Option<OsString>) -> Vec<(&'static str, Option<OsString>)> {
    // `LANGUAGE` picks message catalogs, ahead of `LC_MESSAGES`.
    let mut changes = vec![("LC_MESSAGES", Some(OsString::from("C"))), ("LANGUAGE", None)];
    if let Some(all) = lc_all.filter(|all| !all.is_empty()) {
        changes.push(("LC_ALL", None));
        for category in ["LC_CTYPE", "LC_COLLATE", "LC_NUMERIC", "LC_TIME", "LC_MONETARY"] {
            changes.push((category, Some(all.clone())));
        }
    }
    changes
}

/// The names compilers give what is not a file, in line markers.
const NOT_FILES: &[&[u8]] = &[b"<built-in>", b"<command-line>", b"<command line>", b"<stdin>", b"<scratch space>"];

/// The files a preprocessor run names, each by its mapped name and its
/// path (two files can share a mapped name — `./x.h` in the working
/// directory and `x.h` in the Cactus root — and both are read), with
/// whether the preprocessor entered it.
pub(super) type Named = BTreeMap<(Vec<u8>, PathBuf), bool>;

/// Digest the preprocessor's output as it comes, and collect the files it
/// names.
pub(super) fn digest_output(child: &mut std::process::Child, map: Option<&PathMap>) -> Res<Output> {
    let mut output = BufReader::new(child.stdout.take().context("the preprocessor has no output")?);
    let mut hasher = Hasher::new("text");
    let mut named = Named::new();
    // What `-dU` says the text expands or tests: no other line of `-E`'s
    // output begins so.
    let mut used = BTreeSet::new();
    // The files the text has code of (decision 30), by the name the line
    // markers give them; and the file the lines now come from.
    let mut with_code = BTreeSet::new();
    let mut current: Option<PathBuf> = None;
    let mut front_end_pragma = false;
    let (mut line, mut bytes) = (Vec::new(), 0u64);
    loop {
        line.clear();
        if output.read_until(b'\n', &mut line).context("Failed to read the preprocessor's output")? == 0 {
            break;
        }
        // The length that closes the stream is the length of what went into
        // it: of the mapped text, which does not know how long the
        // installation's path is.
        let mut feed = |part: &[u8]| {
            hasher.stream(part);
            bytes += part.len() as u64;
        };
        match naming(&line, b"# ").map_err(anyhow::Error::msg)? {
            Some(marker) => {
                let mapped = map.map_or_else(|| marker.name.clone(), |map| map.apply(&marker.name));
                feed(marker.head);
                feed(&mapped);
                feed(marker.tail);
                // Not files: the compilers' names for what they made up, by
                // name (a header may be called `<odd>.h`); and a name
                // ending in `//`, GCC's note of the working directory.
                let pseudo = NOT_FILES.contains(&marker.name.as_slice()) || marker.name.ends_with(b"//") || marker.name.is_empty();
                current = Some(PathBuf::from(OsString::from_vec(marker.name.clone())));
                if !pseudo {
                    let entered = marker.enters();
                    *named.entry((mapped, PathBuf::from(OsString::from_vec(marker.name)))).or_default() |= entered;
                }
            }
            None if line.starts_with(b"#pragma GCC pch_preprocess") => {
                bail!("a precompiled header would be used, which the cache does not follow")
            }
            None if assembler_include(&line) => {
                bail!("the source makes the assembler read another file (.incbin or .include), which the cache does not follow")
            }
            None => {
                front_end_pragma |= acted_on_by_the_front_end(&line);
                match definitions(&line).next() {
                    Some(name) => {
                        used.insert(name.to_vec());
                    }
                    None if !line.iter().all(u8::is_ascii_whitespace) => {
                        if let Some(current) = &current {
                            with_code.insert(current.clone());
                        }
                    }
                    None => {}
                }
                feed(&line)
            }
        }
    }
    hasher.end_stream(bytes);
    Ok(Output { text: hasher.hex(), bytes, named, used, with_code, front_end_pragma })
}

/// What [`digest_output`] read of a preprocessor's output: its text
/// digested and its length, the files its markers name, the macros `-dU`
/// says it uses, the files it has code of, and whether it has a `#pragma
/// GCC` the C front end acts on ([`said_as_compiled`]).
pub(super) struct Output {
    pub text: String,
    pub bytes: u64,
    pub named: Named,
    pub used: BTreeSet<Vec<u8>>,
    pub with_code: BTreeSet<PathBuf>,
    pub front_end_pragma: bool,
}

/// Does a C or C++ compile's preprocessor run see the text as the compile
/// does: its macros and every warning it would print? Not for C where the
/// text has a `#pragma GCC` the C front end acts on as it reads on
/// ([`acted_on_by_the_front_end`]), which `-E` ignores or follows only in
/// part: `optimize` (and `target` before GCC 14, with `push_options` and
/// the like) redefines `__OPTIMIZE__`, `__FAST_MATH__`, `__AVX2__` and their
/// kind for the rest of the unit, so `-dU` misses a macro the compile uses
/// (a stale object where its definition is left out); `diagnostic` turns a
/// warning on, or into an error, for the compile alone (`-E` ignores it
/// before GCC 13, and for an option it does not know, `-Wall`, after). For
/// C++ the compile and `-E` agree on every macro and every preprocessor
/// warning after any of them (tried on GCC 8.5, 11.4, 13.2 and 14.2), and
/// libstdc++'s headers are full of them.
pub(super) fn said_as_compiled(language: compile::Language, front_end_pragma: bool) -> bool {
    !(front_end_pragma && language == compile::Language::C)
}

/// Is `line`, of `-E`'s output (which writes every pragma, `_Pragma` too,
/// as `#pragma` at a line's start), a `#pragma GCC` but one of those the
/// preprocessor alone handles, or that change nothing a later line reads?
fn acted_on_by_the_front_end(line: &[u8]) -> bool {
    const LEFT_TO_THE_PREPROCESSOR: [&[u8]; 9] = [b"system_header", b"poison", b"warning", b"error", b"dependency", b"ivdep", b"unroll", b"novector", b"visibility"];
    let mut words = line.split(|b| b.is_ascii_whitespace() || *b == b'(').filter(|word| !word.is_empty());
    words.next() == Some(b"#pragma") && words.next() == Some(b"GCC") && words.next().is_none_or(|word| !LEFT_TO_THE_PREPROCESSOR.contains(&word))
}

/// `said` without the terminal's escape sequences (colors, by
/// `-fdiagnostics-color=always`, and links), which split the words a
/// diagnostic is read by.
pub(super) fn plain(said: &[u8]) -> String {
    let said = String::from_utf8_lossy(said);
    let mut out = String::with_capacity(said.len());
    let mut chars = said.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // A control sequence ends at its final byte.
            Some('[') => {
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
            // An operating system command (a link) ends at BEL or ESC \.
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Is `line`, of what a preprocessor run (with `-v`) said, part of a
/// diagnostic: a warning, an error or a note, as GCC words them in English
/// (`in_english`), or where one came from? `-v`'s own lines (the driver's
/// version and command lines, the search list) are none of these.
fn diagnostic(line: &str) -> bool {
    [": warning: ", ": error: ", ": note: ", ": fatal error: ", ": pedantic: "].iter().any(|kind| line.contains(kind))
        || line.starts_with("In file included from")
        || line.starts_with("                 from ")
        || line.contains(": In function ")
        || line.contains(": In member function ")
        || line.contains(": At top level")
}

/// What the compiler at `path` prints for an unused variable, run here as
/// the compile runs (decision 31's exact warnings): its language and quotes.
fn wording(path: &Path, fortran: bool) -> Res<Vec<u8>> {
    let (language, source) = match fortran {
        true => ("f95", "subroutine f\n  integer :: x\nend subroutine\n"),
        false => ("c", "void f(void) { int x; }\n"),
    };
    let mut child = Command::new(path)
        .args(["-fsyntax-only", "-Wunused-variable", "-x", language, "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("Failed to run {} to see how it words a warning", path.display()))?;
    PREPROCESSOR.store(child.id() as i32, Ordering::SeqCst);
    if let Some(mut stdin) = child.stdin.take() {
        let _ = std::io::Write::write_all(&mut stdin, source.as_bytes());
    }
    let out = child.wait_with_output();
    PREPROCESSOR.store(0, Ordering::SeqCst);
    let out = out.context("Failed to wait for the compiler")?;
    if out.stderr.is_empty() {
        bail!("{} said nothing of an unused variable, so how it words a warning is not known", path.display());
    }
    Ok(out.stderr)
}

/// What a preprocessor run said of the text, as the key has it: its
/// diagnostics, with the build's directories named as the store names them.
/// Whether the compile prints a warning (and with `-Werror` whether it
/// fails) can turn on what no output shows: `__has_include`'s answer
/// deciding a `#warning`, when files were last changed (`#pragma GCC
/// dependency`, however it is spelled), a definition the key leaves out
/// (`#pragma GCC poison` of a defined name), an include directory that is a
/// file. The run says it as the compile does.
pub(super) fn said_of_the_text(said: &[u8], map: Option<&PathMap>) -> Vec<u8> {
    let said = plain(said);
    let lines: Vec<&str> = said.lines().filter(|line| diagnostic(line)).collect();
    // The quotes follow the locale's character set, the words do not.
    let lines = lines.join("\n").replace(['\u{2018}', '\u{2019}'], "'");
    map.map_or_else(|| lines.as_bytes().to_vec(), |map| map.messages_for_the_store(lines.as_bytes()))
}

/// The text part of a key: the digest of a preprocessor run's output
/// `text`, with what the run said of it.
pub(super) fn text_and_said(text: &str, said: &[u8]) -> String {
    let mut hasher = Hasher::new("text and what was said of it");
    hasher.feed(text.as_bytes());
    hasher.feed(said);
    hasher.hex()
}

/// The files a preprocessor run's diagnostics name (where each was said,
/// and the files that included it), which are keyed whole (decision 30): a
/// `#warning` in a unit's source leaves its headers of definitions as they
/// would be otherwise. `None`: a diagnostic names no file (one about the
/// command line, a program's own, or one this cannot read), and every file
/// is keyed whole.
pub(super) fn diagnosed_files(said: &[u8]) -> Option<BTreeSet<PathBuf>> {
    let mut files = BTreeSet::new();
    for line in plain(said).lines().filter(|line| diagnostic(line)) {
        let place = line.strip_prefix("In file included from ").or_else(|| line.strip_prefix("                 from ")).unwrap_or(line).trim_start();
        // The file's name ends at the `:` before a line number, or before
        // " In function" and its like.
        let end = place.char_indices().find(|(at, c)| *c == ':' && (place[at + 1..].starts_with(|d: char| d.is_ascii_digit()) || place[at + 1..].starts_with(" In ") || place[at + 1..].starts_with(" At ")))?.0;
        let name = &place[..end];
        let named = Path::new(name);
        // A name no file has (one a `#line` gave) says nothing of where
        // the lines it speaks of are.
        if name.is_empty() || name.starts_with('<') || name.contains('\x1b') || !named.exists() {
            return None;
        }
        files.insert(named.to_owned());
    }
    Some(files)
}

/// The macros a unit's text expands or tests (`-dU`), and what keeps a
/// definition of another in the key all the same (decision 26).
#[derive(Debug, Clone)]
pub(super) struct Used {
    names: BTreeSet<Vec<u8>>,
    /// Defined or undefined on the command line (`-D`, `-U`): a header's
    /// definition of one draws a warning.
    given: BTreeSet<Vec<u8>>,
    /// The files the text has code of, kept whole (decision 30): what the
    /// compiler checks line by line (indentation between statements, the
    /// columns of declarations) reads their raw lines, definitions too.
    with_code: BTreeSet<PathBuf>,
    /// The files a diagnostic of the preprocessor named, kept whole too, by
    /// physical path (a diagnostic may spell a name otherwise than a marker).
    diagnosed: BTreeSet<PathBuf>,
    /// The unit's own source, keyed whole (`-Wunused-macros` speaks of its
    /// macros).
    pub source: PathBuf,
}

impl Used {
    pub(super) fn new(names: BTreeSet<Vec<u8>>, with_code: BTreeSet<PathBuf>, diagnosed: BTreeSet<PathBuf>, compile: &Compile) -> Self {
        let diagnosed: BTreeSet<PathBuf> = diagnosed.iter().map(|file| std::fs::canonicalize(file).unwrap_or_else(|_| file.clone())).collect();
        let with_code = with_code.into_iter().chain(diagnosed.iter().cloned()).collect();
        let mut used = Self { names, given: BTreeSet::new(), with_code, diagnosed, source: compile.source.clone() };
        let args: Vec<&[u8]> = compile.preprocess.iter().map(|arg| arg.as_bytes()).collect();
        used.given_in(&args);
        used
    }

    /// The macros a command line `words` defines or undefines count as
    /// given too.
    pub(super) fn also_given(&mut self, words: &[String]) {
        let words: Vec<&[u8]> = words.iter().map(|word| word.as_bytes()).collect();
        self.given_in(&words);
    }

    fn given_in(&mut self, args: &[&[u8]]) {
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let value = match arg.strip_prefix(b"-D").or_else(|| arg.strip_prefix(b"-U")) {
                Some(b"") => args.next().copied().unwrap_or_default(),
                Some(value) => value,
                None => continue,
            };
            let name = value.split(|b| *b == b'=' || *b == b'(').next().unwrap_or_default();
            self.given.insert(name.to_vec());
        }
    }

    /// Is the file `path` (by the name the markers give it) kept whole: one
    /// the text has code of, one a diagnostic named, or the source?
    pub(super) fn keeps(&self, path: &Path) -> bool {
        *path == self.source
            || self.with_code.contains(path)
            || (!self.diagnosed.is_empty() && std::fs::canonicalize(path).is_ok_and(|real| self.diagnosed.contains(&real)))
    }

    /// May a definition of `name` be left out of a file's bytes? Not if the
    /// text uses it, the command line gives it, or the name is reserved
    /// (the compiler's own, which a header redefines with a warning).
    pub(super) fn droppable(&self, name: &[u8]) -> bool {
        let reserved = name.starts_with(b"__") || (name.first() == Some(&b'_') && name.get(1).is_some_and(u8::is_ascii_uppercase));
        !reserved && !self.names.contains(name) && !self.given.contains(name)
    }
}

/// The logical lines of `bytes` (physical lines joined where a backslash
/// ends one), each with its line breaks.
fn logical_lines(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut at = 0;
    std::iter::from_fn(move || {
        if at >= bytes.len() {
            return None;
        }
        let start = at;
        loop {
            let end = bytes[at..].iter().position(|b| *b == b'\n').map_or(bytes.len(), |nl| at + nl + 1);
            let continued = end > start + 1 && bytes[end - 1] == b'\n' && bytes[end - 2] == b'\\';
            at = end;
            if !continued || at >= bytes.len() {
                break;
            }
        }
        Some(&bytes[start..at])
    })
}

/// Does `bytes` hold a `#pragma GCC dependency` (blanks allowed between
/// its words)?
fn pragma_dependency(bytes: &[u8]) -> bool {
    logical_lines(bytes).any(|line| {
        let mut words = line.split(|b| b.is_ascii_whitespace()).filter(|word| !word.is_empty());
        let first = words.next().unwrap_or_default();
        let rest: Vec<&[u8]> = match first {
            b"#" => words.take(3).collect(),
            _ if first.starts_with(b"#") => std::iter::once(&first[1..]).chain(words.take(2)).collect(),
            _ => return false,
        };
        rest == [&b"pragma"[..], b"GCC", b"dependency"]
    })
}

/// The macro a line defines or undefines (`#define NAME`, `#undef NAME`,
/// blanks allowed around the `#`), if it is such a line.
fn definition(line: &[u8]) -> Option<&[u8]> {
    let blank = |b: &u8| *b == b' ' || *b == b'\t';
    let skip = |bytes: &[u8]| -> usize { bytes.iter().take_while(|b| blank(b)).count() };
    let rest = &line[skip(line)..];
    let rest = rest.strip_prefix(b"#")?;
    let rest = &rest[skip(rest)..];
    let rest = rest.strip_prefix(b"define").or_else(|| rest.strip_prefix(b"undef"))?;
    let gap = skip(rest);
    if gap == 0 {
        return None;
    }
    let rest = &rest[gap..];
    let len = rest.iter().take_while(|b| b.is_ascii_alphanumeric() || **b == b'_').count();
    (len > 0 && !rest[0].is_ascii_digit()).then(|| &rest[..len])
}

/// Where a line of C or C++ begins: in code, in a block comment, or in a
/// raw string literal (with its delimiter).
#[derive(Debug, Clone, PartialEq)]
enum Lex {
    Code,
    Comment,
    Raw(Vec<u8>),
}

/// Where the line after `line` begins, `line` beginning in `state`. Strings
/// and character constants end with their line; one that does not is cut
/// off there, as a compiler would complain of it.
fn lex_line(line: &[u8], mut state: Lex) -> Lex {
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut at = 0;
    while at < line.len() {
        match &state {
            Lex::Comment => match line[at..].starts_with(b"*/") {
                true => {
                    state = Lex::Code;
                    at += 2;
                }
                false => at += 1,
            },
            Lex::Raw(delimiter) => {
                let close = [b")", delimiter.as_slice(), b"\""].concat();
                match line[at..].starts_with(&close) {
                    true => {
                        at += close.len();
                        state = Lex::Code;
                    }
                    false => at += 1,
                }
            }
            Lex::Code => match line[at] {
                b'/' if line[at..].starts_with(b"//") => return Lex::Code,
                b'/' if line[at..].starts_with(b"/*") => {
                    state = Lex::Comment;
                    at += 2;
                }
                b'"' => {
                    // `R"`, `u8R"`, `uR"`, `UR"`, `LR"`: a raw string,
                    // whose delimiter runs to the `(`.
                    let before = &line[..at];
                    let prefix = before.strip_suffix(b"R").map(|before| {
                        let before = [b"u8".as_slice(), b"u", b"U", b"L"].iter().find_map(|p| before.strip_suffix(*p)).unwrap_or(before);
                        before.last().is_none_or(|b| !ident(*b))
                    });
                    match (prefix, line[at + 1..].iter().position(|b| *b == b'(')) {
                        (Some(true), Some(open)) => {
                            state = Lex::Raw(line[at + 1..at + 1 + open].to_vec());
                            at += open + 2;
                        }
                        _ => {
                            at += 1;
                            while at < line.len() && line[at] != b'"' {
                                at += if line[at] == b'\\' { 2 } else { 1 };
                            }
                            at += 1;
                        }
                    }
                }
                // A digit separator (`1'024`, `0xff'ff`): the quote stands in
                // a number, which begins with a digit.
                b'\'' if {
                    let token = line[..at].iter().rev().take_while(|b| ident(**b) || **b == b'\'').count();
                    line[at - token..at].first().is_some_and(u8::is_ascii_digit)
                } =>
                {
                    at += 1
                }
                b'\'' => {
                    at += 1;
                    while at < line.len() && line[at] != b'\'' {
                        at += if line[at] == b'\\' { 2 } else { 1 };
                    }
                    at += 1;
                }
                _ => at += 1,
            },
        }
    }
    state
}

/// The logical lines of `bytes`, each with the macro it defines or
/// undefines if it is such a line: one that begins in code (not inside a
/// block comment or a raw string).
fn directives(bytes: &[u8]) -> impl Iterator<Item = (&[u8], Option<&[u8]>)> {
    let mut state = Lex::Code;
    logical_lines(bytes).map(move |line| {
        let defined = match state {
            Lex::Code => definition(line),
            _ => None,
        };
        state = lex_line(line, std::mem::replace(&mut state, Lex::Code));
        (line, defined)
    })
}

/// Every macro `bytes` defines or undefines, once per line that does.
fn definitions(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    directives(bytes).filter_map(|(_, defined)| defined)
}

/// `bytes` without the lines that define or undefine a macro `drop` says
/// to leave out (decision 29: line breaks and all).
fn without_definitions(bytes: &[u8], drop: impl Fn(&[u8]) -> bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for (line, defined) in directives(bytes) {
        if !defined.is_some_and(&drop) {
            out.extend_from_slice(line);
        }
    }
    out
}

/// The files a dependency file as GCC writes it (`-MD`) names: what follows
/// the target's colon, a backslash before a line break joining lines, a
/// backslash before a blank or `#` escaping it, `$$` for `$`; and after it,
/// with `-MP`, a rule of no prerequisites for each header, which names
/// nothing new. `None`: not one such file.
pub(super) fn depend_files(said: &[u8]) -> Option<Vec<PathBuf>> {
    let text = std::str::from_utf8(said).ok()?.replace("\\\n", " ");
    // The first rule, up to the first line break left; the rest must be
    // `-MP`'s empty rules.
    let (first, phony) = text.split_once('\n').unwrap_or((&text, ""));
    if !phony.lines().all(|line| line.trim().is_empty() || (line.trim_end().ends_with(':') && !line.trim_end()[..line.trim_end().len() - 1].contains(": "))) {
        return None;
    }
    let (_, rest) = first.split_once(": ")?;
    let mut files = Vec::new();
    let mut file = String::new();
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if matches!(chars.peek(), Some(' ' | '#' | '\\')) => file.push(chars.next()?),
            '$' if chars.peek() == Some(&'$') => {
                chars.next();
                file.push('$');
            }
            c if c.is_whitespace() => {
                if !file.is_empty() {
                    files.push(PathBuf::from(std::mem::take(&mut file)));
                }
            }
            // A second target or a rule: not what `-MD` writes alone.
            ':' if chars.peek().is_none_or(|next| next.is_whitespace()) => return None,
            c => file.push(c),
        }
    }
    if !file.is_empty() {
        files.push(PathBuf::from(file));
    }
    Some(files)
}

/// Where a compile says what it read (decision 28): beside its object, so
/// that it can write there, under a short name of this process's own (one
/// made of the object's name could pass the longest name a directory takes
/// where the object's does not).
pub(super) fn reads_file(output: &Path) -> PathBuf {
    let dir = output.parent().filter(|dir| !dir.as_os_str().is_empty()).unwrap_or(Path::new("."));
    dir.join(format!(".cactup-{}.d", std::process::id()))
}

/// Did a compile read what its key read? `read`, what it says it read
/// (its dependency output), against `keyed`, the files the key's
/// preprocessor runs entered: the same files, by name or, where the names
/// differ, by where they lead (decision 28: a file a compile could not see
/// for a moment, as on a node whose file system stalled, takes another
/// branch of a `__has_include` and makes another object, while the key's
/// runs, before and after, saw it).
pub(super) fn reads_match(read: &[PathBuf], keyed: &BTreeSet<PathBuf>) -> bool {
    let read: BTreeSet<&Path> = read.iter().map(PathBuf::as_path).collect();
    let keyed: BTreeSet<&Path> = keyed.iter().map(PathBuf::as_path).collect();
    if read == keyed {
        return true;
    }
    let real = |paths: Vec<&&Path>| -> Option<BTreeSet<PathBuf>> { paths.into_iter().map(|path| std::fs::canonicalize(path).ok()).collect() };
    match (real(read.difference(&keyed).collect()), real(keyed.difference(&read).collect())) {
        (Some(only_read), Some(only_keyed)) => only_read == only_keyed,
        _ => false,
    }
}

/// Why the driver, run as `name` with the environment `env` gives, could
/// find other programs and files than the key and the checks see. Its own
/// prefixes named relatively (an empty entry is the working directory) lead
/// to programs the compiler's identity does not watch, from one directory
/// to the next; a library loaded or audited by a relative path is one no
/// search shows. (A search for the compiler's programs, libraries or
/// `specs` file through a relative entry of `PATH`, `LD_LIBRARY_PATH` or
/// `LIBRARY_PATH` is watched from each compile's own directory, decision
/// 22.) A Fortran compile's dependency run runs in a directory of its own:
/// for it, also where its driver finds itself (by the name it is run by, or
/// along `PATH` before it is found). A hit
/// compares nothing after the key: so these keep the compile out of the
/// cache.
fn driver_led_elsewhere(name: &OsStr, fortran: bool, env: impl Fn(&str) -> Option<OsString>) -> Option<String> {
    let relative_entry = |variable: &str| {
        env(variable).is_some_and(|value| value.as_bytes().split(|b| *b == b':').any(|entry| !entry.starts_with(b"/")))
    };
    let bare = !name.as_bytes().contains(&b'/');
    if fortran && !bare && !name.as_bytes().starts_with(b"/") {
        return Some("the compiler is named by a relative path".to_owned());
    }
    // (A relative entry of `PATH` or `LD_LIBRARY_PATH` that a search for the
    // compiler's programs or libraries passes through rules the compiler
    // out: `identity`.) A Fortran driver named without a `/` finds its own
    // prefix along `PATH` from the dependency run's directory too.
    let variables = ["COMPILER_PATH", "GCC_EXEC_PREFIX"];
    // A driver named without a `/` walks `PATH` for itself, in order, up to
    // the first file of its name: a relative entry before that is another
    // place from the dependency run's directory.
    if fortran && bare && let Some(path) = env("PATH") {
        for entry in path.as_bytes().split(|b| *b == b':') {
            if !entry.starts_with(b"/") {
                return Some("PATH has an entry that is not an absolute path (an empty one is the working directory) before the compiler".to_owned());
            }
            let candidate = Path::new(OsStr::from_bytes(entry)).join(name);
            if candidate.metadata().is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0) {
                break;
            }
        }
    }
    // A name that is not UTF-8 cannot be remembered with the identity, which
    // would then be worked out afresh for every compile.
    for variable in ["PATH", "LD_LIBRARY_PATH", "LIBRARY_PATH", "COMPILER_PATH", "GCC_EXEC_PREFIX", "LD_PRELOAD"] {
        if env(variable).is_some_and(|value| value.to_str().is_none()) {
            return Some(format!("{variable} has an entry whose name is not UTF-8"));
        }
    }
    if let Some(variable) = variables.into_iter().find(|variable| relative_entry(variable)) {
        return Some(format!("{variable} has an entry that is not an absolute path (an empty one is the working directory)"));
    }
    // A preloaded library named without a `/` is searched for, as any other
    // (one named through a token the loader expands is watched where the
    // loader would look: `identity`).
    let relative_file = |entry: &[u8]| entry.contains(&b'/') && !entry.starts_with(b"/");
    if env("LD_PRELOAD").is_some_and(|value| value.as_bytes().split(|b| *b == b':' || *b == b' ').any(relative_file)) {
        return Some("LD_PRELOAD names a library by a relative path".to_owned());
    }
    env("LD_AUDIT").filter(|value| !value.is_empty()).map(|_| "LD_AUDIT loads libraries the cache does not follow".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objcache::Mode;

    fn map(pairs: &[(&str, &'static str)]) -> PathMap {
        let mut from_to: Vec<(Vec<u8>, &'static str)> = pairs.iter().map(|(from, to)| (from.as_bytes().to_vec(), *to)).collect();
        from_to.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
        PathMap { from_to }
    }

    fn applied(map: &PathMap, name: &str) -> String {
        String::from_utf8(map.apply(name.as_bytes())).unwrap()
    }

    #[test]
    fn maps_names_where_they_begin_in_a_word() {
        let map = map(&[("/w/Cactus/", ROOT_NAME), ("/w/Cactus/configs/sim/", CONFIG_NAME)]);
        let mapped = |text: &str| String::from_utf8(map.in_text(text.as_bytes())).unwrap();
        assert_eq!(mapped("/w/Cactus/configs/sim/build/a.o.cpp1.ii"), "/cactup-root/configs/@config/build/a.o.cpp1.ii");
        assert_eq!(mapped("-I/w/Cactus/configs/sim/bindings/include"), "-I/cactup-root/configs/@config/bindings/include");
        assert_eq!(mapped("--orig_src_path_name=/w/Cactus/src/a.cu"), "--orig_src_path_name=/cactup-root/src/a.cu");
        assert_eq!(mapped("\"-I/w/Cactus/x\" \"-I/w/Cactus/y\""), "\"-I/cactup-root/x\" \"-I/cactup-root/y\"");
        // Not inside another name.
        assert_eq!(mapped("/x/w/Cactus/a"), "/x/w/Cactus/a");
        assert_eq!(mapped("-I/v/w/Cactus/a"), "-I/v/w/Cactus/a");
    }

    #[test]
    fn maps_names_as_the_compiler_does() {
        let map = map(&[("/w/Cactus/", ROOT_NAME), ("/w/Cactus/configs/sim/", CONFIG_NAME)]);
        assert_eq!(applied(&map, "/w/Cactus/arrangements/A/T/src/a.c"), "/cactup-root/arrangements/A/T/src/a.c");
        assert_eq!(applied(&map, "/w/Cactus/configs/sim/build/T/a.c"), "/cactup-root/configs/@config/build/T/a.c");
        // Another configuration is under the root, not this configuration.
        assert_eq!(applied(&map, "/w/Cactus/configs/simple/x"), "/cactup-root/configs/simple/x");
        // A prefix of the name, and only at its start; a directory that
        // merely begins alike is another directory.
        for untouched in ["/w/Cactus-libs/include/x.h", "/w/Cactus", "/opt/w/Cactus/x.h", "x/w/Cactus/y", "relative.h"] {
            assert_eq!(applied(&map, untouched), untouched);
        }
        // The flags name the less specific directory first.
        let flags: Vec<String> = map.flags().into_iter().map(|f| f.into_string().unwrap()).collect();
        assert_eq!(flags, ["-ffile-prefix-map=/w/Cactus/=/cactup-root/", "-ffile-prefix-map=/w/Cactus/configs/sim/=/cactup-root/configs/@config/"]);
        // The trial's flags are spelled by the same code.
        assert_eq!(trial_flags(Path::new("/w/Cactus"), Path::new("/w/Cactus/configs/sim")), map.flags());
    }

    #[test]
    fn takes_line_markers_apart() {
        let marker = |line: &str| {
            let marker = naming(line.as_bytes(), b"# ").unwrap()?;
            let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
            Some((text(marker.head), text(&marker.name), text(marker.tail), marker.enters()))
        };
        assert_eq!(marker("# 12 \"a.h\" 1\n"), Some(("# 12 \"".into(), "a.h".into(), "\" 1\n".into(), true)));
        assert_eq!(marker("# 12 \"a.h\" 1 3 4\n").unwrap().3, true);
        assert_eq!(marker("# 12 \"a.h\" 2\n").unwrap().3, false);
        assert_eq!(marker("# 12 \"a.h\" 3\n").unwrap().3, false);
        assert_eq!(marker("# 0 \"<built-in>\"\n"), Some(("# 0 \"".into(), "<built-in>".into(), "\"\n".into(), false)));
        // Escapes in a name are undone: the ones GCC and Clang write (a
        // backslash, a quote, octal for a tab or a byte outside ASCII), and
        // the rest of C's.
        assert_eq!(marker("# 1 \"a\\\\b\\\"c.h\"\n").unwrap().1, "a\\b\"c.h");
        assert_eq!(marker("# 1 \"biblioth\\303\\250que/a\\011b\\tc.h\" 1\n").unwrap().1, "bibliothèque/a\tb\tc.h");
        assert_eq!(marker("# 1 \"a\\x41\\0b\\7.h\"\n").unwrap().1, "aA\0b\u{7}.h");
        for not in ["#define X 1\n", "#pragma once\n", "# pragma omp\n", "int x; # 1 \"a\"\n", "#\n", "", "# 1\n", "# x \"a.h\"\n"] {
            assert_eq!(marker(not), None, "{not:?}");
        }
        // What begins like a marker and cannot be read is an error, never
        // "no file named here".
        for unreadable in ["# 1 \"open\n", "# 1 \"a\\qb.h\"\n", "# 1 \"a\\777.h\"\n", "# 1 \"a\\x.h\"\n", "# 1 \"a\\"] {
            assert!(naming(unreadable.as_bytes(), b"# ").is_err(), "{unreadable:?}");
        }
    }

    #[test]
    fn a_first_line_that_names_the_original_is_keyed_by_its_mapped_name() {
        let map = map(&[("/w/Cactus/", ROOT_NAME), ("/v/elsewhere/Cactus/", ROOT_NAME)]);
        let tmp = tempfile::tempdir().unwrap();
        let digest = |content: &str, map: Option<&PathMap>| {
            let file = tmp.path().join("copy.c");
            std::fs::write(&file, content).unwrap();
            content_digest(&std::fs::read(&file).unwrap(), map)
        };
        let here = "#line 1 \"/w/Cactus/arrangements/A/T/src/a.c\"\nint a;\n";
        let there = "#line 1 \"/v/elsewhere/Cactus/arrangements/A/T/src/a.c\"\nint a;\n";
        assert_eq!(digest(here, Some(&map)), digest(there, Some(&map)));
        // Without a map, bytes are bytes.
        assert_ne!(digest(here, None), digest(there, None));
        assert_eq!(digest("int a;\n", None), crate::objcache::hash::bytes_digest(b"int a;\n"));
        // Another original, another line, anything after the first line:
        // all of it still counts.
        for other in [
            "#line 1 \"/w/Cactus/arrangements/A/T/src/b.c\"\nint a;\n",
            "#line 2 \"/w/Cactus/arrangements/A/T/src/a.c\"\nint a;\n",
            "#line 1 \"/w/Cactus/arrangements/A/T/src/a.c\"\nint b;\n",
            "#line 1 \"/w/Cactus/arrangements/A/T/src/a.c\" \nint a;\n",
            "\n#line 1 \"/w/Cactus/arrangements/A/T/src/a.c\"\nint a;\n",
            "int a;\n",
        ] {
            assert_ne!(digest(other, Some(&map)), digest(here, Some(&map)), "{other:?}");
        }
        // Only the first line is read that way: a path further down may be
        // text, and is kept as it is.
        let below = |root: &str| format!("int a;\n/*\n#line 1 \"{root}/x.c\"\n*/\n");
        assert_ne!(digest(&below("/w/Cactus"), Some(&map)), digest(&below("/v/elsewhere/Cactus"), Some(&map)));
    }

    #[test]
    fn the_preprocessor_is_not_asked_for_the_compiles_dependency_file() {
        // Both preprocessor runs, before the compile and after it, are this
        // command. Nothing of the flags that write a dependency file may be
        // in it: the file is the compile's to write, once.
        let os = |args: &[&str]| args.iter().map(OsString::from).collect::<Vec<_>>();
        let depend = ["-MD", "-MMD", "-MP", "-MF", "/c/build/T/a.c.d", "-MT", "a.c.o", "-MQ", "b.o"];
        let mut args = os(&["-g3", "-O2", "-c", "-o", "/c/build/T/a.c.o", "/c/build/T/a.c", "-I/src/T", "-DX=1"]);
        args.splice(2..2, os(&depend));
        let compile = compile::parse(&args).unwrap();
        assert_eq!(compile.depend, os(&depend));
        for family in [Family::Gcc, Family::Clang] {
            let compiler = Compiler { path: PathBuf::from("/usr/bin/cc"), family, relocates: true, locale_neutral: true, id: String::new(), multilib: Vec::new(), specs: None, takes_module_id: false, toolchain: Vec::new(), host_reads: Vec::new() };
            let command = preprocessor(&compiler, OsStr::new("cc"), &compile, None, None);
            let given: Vec<&OsStr> = command.get_args().collect();
            assert!(given.contains(&OsStr::new("-E")) && given.contains(&OsStr::new("/c/build/T/a.c")), "{given:?}");
            assert!(!given.iter().any(|arg| arg.as_bytes().starts_with(b"-M") || depend.contains(&arg.to_str().unwrap())), "{given:?}");
        }
    }

    /// The key's run writes the dependency file the compile names last, to
    /// a temporary name, and names the target the compile would name; a
    /// value that looks like a flag is a value.
    #[test]
    fn the_dependency_flags_of_the_key_run() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().display();
        let flags = |args: &str| {
            let args: Vec<OsString> = args.split(' ').map(|arg| OsString::from(arg.replace("DIR", &dir.to_string()))).collect();
            let compile = compile::parse(&args).unwrap();
            let ((temp, real), flags) = depend_flags(&compile).unwrap().unwrap();
            let shown: Vec<String> = flags.iter().map(|f| f.to_string_lossy().replace(&temp.display().to_string(), "TEMP").replace(&dir.to_string(), "DIR")).collect();
            (shown.join(" "), real.display().to_string().replace(&dir.to_string(), "DIR"))
        };
        assert_eq!(flags("-c -o o.o a.c -MD -MF DIR/a.d"), ("-MD -MF TEMP -MQ o.o".to_owned(), "DIR/a.d".to_owned()));
        assert_eq!(flags("-c -o o.o a.c -MD -MF DIR/x.d -MF DIR/a.d -MP"), ("-MD -MF DIR/x.d -MF TEMP -MP -MQ o.o".to_owned(), "DIR/a.d".to_owned()));
        // `-MT -MF`: the target is called "-MF".
        assert_eq!(flags("-c -o o.o a.c -MD -MF DIR/a.d -MT -MF"), ("-MD -MF TEMP -MT -MF".to_owned(), "DIR/a.d".to_owned()));
        assert_eq!(flags("-c -o o.o a.c -MD -MT -MF -MF DIR/a.d -MQ x"), ("-MD -MT -MF -MF TEMP -MQ x".to_owned(), "DIR/a.d".to_owned()));
    }

    /// A key changes only on purpose: this pins the digest of fixed parts.
    /// If it fails, the way a key is made has changed; see [`KEY_LABEL`] for
    /// when that needs a new label, and then update the digest here.
    #[test]
    fn the_key_of_fixed_parts_is_pinned() {
        let parts = Parts {
            platform: "p".into(),
            compiler: "c".into(),
            arguments: "a".into(),
            environment: "e".into(),
            text: "t".into(),
            files: "f".into(),
        };
        assert_eq!(parts.key(), "4c996124dea19d194cf2d14205cdacd60986f15684f21f33e08b368ba813ee78");
        assert_eq!(PathMap::description(), ["-ffile-prefix-map", "/cactup-root/", "/cactup-root/configs/@config/"]);
    }

    /// Messages keep the map's directories as tokens where a path begins,
    /// and only there, and come back as this build's.
    #[test]
    fn messages_travel_with_the_tree_they_name() {
        let map = map(&[("/w/Cactus/configs/sim/", CONFIG_NAME), ("/w/Cactus/", ROOT_NAME)]);
        let said = b"/w/Cactus/arrangements/A/T/src/a.c:3: warning: x\nIn file included from /w/Cactus/configs/sim/bindings/h.h,\n/x/w/Cactus/y and '/w/Cactus/z'\n\x1b[01m\x1b[K/w/Cactus/c.c:\x1b[m\x1b[K\n";
        let stored = map.messages_for_the_store(said);
        assert_eq!(
            String::from_utf8_lossy(&stored),
            "@CACTUP_ROOT@/arrangements/A/T/src/a.c:3: warning: x\nIn file included from @CACTUP_CONFIG@/bindings/h.h,\n/x/w/Cactus/y and '@CACTUP_ROOT@/z'\n\x1b[01m\x1b[K@CACTUP_ROOT@/c.c:\x1b[m\x1b[K\n"
        );
        let conf = BuildConf {
            mode: Mode::Serve,
            cactup: PathBuf::from("/c"),
            config_dir: PathBuf::from("/v/Cactus/configs/other"),
            cactus_root: PathBuf::from("/v/Cactus"),
            machine: "m".into(),
            universe: None,
            build_env_digest: String::new(),
            store: PathBuf::from("/s"),
            relocate: true,
            exact_warnings: false,
            size_limit: None,
        };
        assert_eq!(
            String::from_utf8_lossy(&messages_for_this_build(&conf, &stored)),
            "/v/Cactus/arrangements/A/T/src/a.c:3: warning: x\nIn file included from /v/Cactus/configs/other/bindings/h.h,\n/x/w/Cactus/y and '/v/Cactus/z'\n\x1b[01m\x1b[K/v/Cactus/c.c:\x1b[m\x1b[K\n"
        );
    }

    #[test]
    fn hears_a_driver_say_where_else_it_takes_flags_from() {
        let gcc = |said: &str| flags_from_elsewhere(Family::Gcc, None, said.as_bytes());
        assert_eq!(gcc("Using built-in specs.\nCOLLECT_GCC=gcc\nTarget: x86_64-linux-gnu\n"), Ok(()));
        assert!(gcc("Reading specs from /opt/gcc/lib/gcc/x86_64-linux-gnu/14/specs\nCOLLECT_GCC=gcc\n").unwrap_err().contains("/14/specs"));
        // Both, as with `-specs=`: one file read is one too many.
        assert!(gcc("Using built-in specs.\nReading specs from extra.specs\n").is_err());
        // Silence, or another language, is not "built-in".
        assert!(gcc("").unwrap_err().contains("does not say"));
        assert!(gcc("Es werden eingebaute Spezifikationen verwendet.\n").is_err());

        // A GCC whose specs file was accepted reads that file, by whatever
        // path, and no other.
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("lib");
        for dir in [&lib, &tmp.path().join("bin")] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(lib.join("specs"), "").unwrap();
        std::fs::write(tmp.path().join("other.specs"), "").unwrap();
        let accepted = std::fs::canonicalize(lib.join("specs")).unwrap();
        let with = |said: String| flags_from_elsewhere(Family::Gcc, Some(&accepted), said.as_bytes());
        let reading = |file: &Path| format!("Reading specs from {}\nCOLLECT_GCC=gcc\n", file.display());
        assert_eq!(with(reading(&tmp.path().join("bin/../lib/specs"))), Ok(()));
        assert!(with(reading(&tmp.path().join("other.specs"))).unwrap_err().contains("other.specs"));
        assert!(with(reading(&accepted) + &reading(&tmp.path().join("other.specs"))).unwrap_err().contains("other.specs"));
        assert!(with("Using built-in specs.\n".to_owned()).unwrap_err().contains("does not say"));
        assert!(gcc(&reading(&accepted)).unwrap_err().contains("reads a specs file"));

        // A translated GCC is asked in English, with the rest of the
        // locale left alone.
        assert_eq!(english_messages(None), [("LC_MESSAGES", Some("C".into())), ("LANGUAGE", None)]);
        let taken_apart = english_messages(Some("de_DE.UTF-8".into()));
        assert!(taken_apart.contains(&("LC_ALL", None)) && taken_apart.contains(&("LC_CTYPE", Some("de_DE.UTF-8".into()))));
        assert!(taken_apart.contains(&("LC_MESSAGES", Some("C".into()))));
        // An empty `LC_ALL` overrides nothing, and nothing is put in its place.
        assert_eq!(english_messages(Some("".into())), english_messages(None));

        let clang = |said: &str| flags_from_elsewhere(Family::Clang, None, said.as_bytes());
        assert!(clang("").unwrap_err().contains("does not say"));
        assert!(clang("clang version 19.1.7\nTarget: x86_64-pc-linux-gnu\n").is_err());
        for cc1 in [" \"/usr/lib/llvm-19/bin/clang\" -cc1 -triple x86_64-pc-linux-gnu -E\n", " \"/usr/bin/clang\" \"-cc1\" \"-triple\" \"x86_64\"\n"] {
            assert_eq!(clang(&format!("clang version 19.1.7\nTarget: x86_64-pc-linux-gnu\nInstalledDir: /usr/bin\n{cc1}")), Ok(()), "{cc1}");
        }
        // Cut short after `InstalledDir:`, where a configuration file would
        // have been named next: no answer.
        assert!(clang("clang version 19.1.7\nTarget: x86_64-pc-linux-gnu\nInstalledDir: /usr/bin\n").is_err());
        let err = clang("clang version 19.1.7\nTarget: i386-pc-linux-gnu\nConfiguration file: /opt/bin/i386-pc-linux-gnu-clang.cfg\n");
        assert!(err.unwrap_err().contains("i386-pc-linux-gnu-clang.cfg"));
    }

    #[test]
    fn notices_assembler_includes() {
        for line in ["__asm__(\".incbin \\\"blob.bin\\\"\");", "asm(\".include \\\"more.s\\\"\");", " .incbin \"x\"", ".incbin\t\"x\""] {
            assert!(assembler_include(line.as_bytes()), "{line}");
        }
        for line in ["set.include(x);", "a.includes(b)", "// .incbin is not used", "x.include_dirs = 1;", "int incbin;"] {
            assert!(!assembler_include(line.as_bytes()), "{line}");
        }
    }

    #[test]
    fn every_part_is_in_the_key() {
        let parts = Parts {
            platform: "p".into(),
            compiler: "c".into(),
            arguments: "a".into(),
            environment: "e".into(),
            text: "t".into(),
            files: "f".into(),
        };
        let key = parts.key();
        for change in [
            |p: &mut Parts| p.platform.push('x'),
            |p: &mut Parts| p.compiler.push('x'),
            |p: &mut Parts| p.arguments.push('x'),
            |p: &mut Parts| p.environment.push('x'),
            |p: &mut Parts| p.text.push('x'),
            |p: &mut Parts| p.files.push('x'),
        ] {
            let mut changed = parts.clone();
            change(&mut changed);
            assert_ne!(changed.key(), key);
        }
    }

    const HEADER: &str = "#define ANSWER 42 /* the answer */\n";
    const SOURCE: &str = "#include \"t.h\"\nconst char *file = __FILE__;\nint answer = ANSWER;\n";

    /// A tree with one source and one header, in a Cactus-like layout.
    struct Tree {
        _tmp: tempfile::TempDir,
        conf: BuildConf,
        cc: PathBuf,
    }

    impl Tree {
        fn new() -> Self {
            Self::named("sim")
        }

        fn named(config: &str) -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(tmp.path()).unwrap().join("Cactus");
            let config = root.join("configs").join(config);
            let cc = config.join(".cactup-builds/0000/cc");
            for dir in [&cc, &config.join("build/T"), &config.join("scratch"), &root.join("arrangements/A/T/src")] {
                std::fs::create_dir_all(dir).unwrap();
            }
            let conf = BuildConf {
                mode: Mode::Record,
                cactup: PathBuf::from("/opt/cactup"),
                config_dir: config,
                cactus_root: root,
                machine: "test".into(),
                universe: None,
                build_env_digest: String::new(),
                store: PathBuf::from("/nonexistent/cache"),
                relocate: true,
                exact_warnings: false,
                size_limit: None,
            };
            let tree = Self { _tmp: tmp, conf, cc };
            std::fs::write(tree.header(), HEADER).unwrap();
            std::fs::write(tree.source(), SOURCE).unwrap();
            tree
        }

        fn header(&self) -> PathBuf {
            self.conf.cactus_root.join("arrangements/A/T/src/t.h")
        }

        fn source(&self) -> PathBuf {
            self.conf.config_dir.join("build/T/a.c")
        }

        fn argv(&self, compiler: &str, flags: &[&str]) -> Vec<OsString> {
            let mut argv = vec![OsString::from(compiler)];
            argv.extend(flags.iter().map(OsString::from));
            argv.extend([OsString::from("-c"), "-o".into(), self.source().with_extension("c.o").into(), self.source().into()]);
            argv.push(format!("-I{}", self.header().parent().unwrap().display()).into());
            argv
        }

        /// Keyed, and got ready for the compile, as the wrapper does.
        fn key(&self, flags: &[&str]) -> Keyed {
            let mut keyed = key(&self.conf, &self.cc, &self.argv("gcc", flags), false).unwrap();
            keyed.before_compile();
            keyed
        }

        fn why_not(&self, flags: &[&str]) -> String {
            key(&self.conf, &self.cc, &self.argv("gcc", flags), false).unwrap_err()
        }
    }

    fn have(compiler: &str, family: Family) -> bool {
        let found = identity::identify(&std::env::temp_dir().join("cactup-test-compilers"), OsStr::new(compiler));
        let have = found.is_ok_and(|c| c.family == family);
        if !have {
            eprintln!("skipped: no {compiler} on this host");
        }
        have
    }

    #[test]
    fn the_same_sources_in_another_installation_key_the_same() {
        if !have("gcc", Family::Gcc) {
            return;
        }
        let (here, there, renamed) = (Tree::new(), Tree::new(), Tree::named("sim-debug"));
        assert_ne!(here.conf.cactus_root, there.conf.cactus_root);
        let mut a = here.key(&["-O2", "-g"]);
        assert!(a.relocatable() && a.files >= 2, "the source and its header, and what the compiler includes by itself");
        assert_eq!(there.key(&["-O2", "-g"]).parts, a.parts, "nothing in the key may know where the tree is");
        assert_eq!(renamed.key(&["-O2", "-g"]).parts, a.parts, "nor what the configuration is called");
        assert!(a.still_holds());
    }

    /// The check after the compile holds for files that were left alone,
    /// and not for one that was changed and changed back meanwhile: its
    /// bytes are the keyed ones again, but the compile may have read the
    /// others.
    #[test]
    fn a_file_changed_and_changed_back_does_not_pass_the_check() {
        if !have("gcc", Family::Gcc) {
            return;
        }
        let tree = Tree::new();
        let mut keyed = tree.key(&["-O2"]);
        assert!(keyed.still_holds());
        let header = std::fs::read(tree.header()).unwrap();
        std::fs::write(tree.header(), b"#define CHANGED 1\n").unwrap();
        std::fs::write(tree.header(), &header).unwrap();
        assert_eq!(std::fs::read(tree.header()).unwrap(), header);
        assert!(!keyed.still_holds(), "a header written over in between passed the check");
        // Keyed again, it holds again.
        assert!(tree.key(&["-O2"]).still_holds());
    }

    /// The same for a directory on the way to a header: an include
    /// directory that is a symlink, switched to another tree and back as a
    /// package manager's view switches (a new link renamed over the old).
    #[test]
    fn a_symlinked_directory_switched_and_switched_back_does_not_pass_the_check() {
        if !have("gcc", Family::Gcc) {
            return;
        }
        let tree = Tree::new();
        let src = tree.header().parent().unwrap().to_owned();
        let (a, b) = (src.with_file_name("src-a"), src.with_file_name("src-b"));
        std::fs::rename(&src, &a).unwrap();
        std::fs::create_dir(&b).unwrap();
        std::fs::write(b.join("t.h"), "#define ANSWER 43\n").unwrap();
        let switch = |to: &Path| {
            let new = src.with_file_name("src.new");
            std::os::unix::fs::symlink(to.file_name().unwrap(), &new).unwrap();
            std::fs::rename(&new, &src).unwrap();
        };
        switch(&a);
        let mut keyed = tree.key(&["-O2"]);
        assert!(keyed.still_holds());
        switch(&b);
        switch(&a);
        assert!(!keyed.still_holds(), "an include directory switched away and back passed the check");
        assert!(tree.key(&["-O2"]).still_holds());
    }

    #[test]
    fn what_changes_the_object_changes_the_key() {
        if !have("gcc", Family::Gcc) {
            return;
        }
        let tree = Tree::new();
        let mut base = tree.key(&["-O2"]);

        // A flag that decides code generation: the arguments differ.
        let optimized = tree.key(&["-O3"]);
        assert_ne!(optimized.parts.arguments, base.parts.arguments);
        assert_eq!((&optimized.parts.text, &optimized.parts.files), (&base.parts.text, &base.parts.files));
        // A macro on the command line that the source uses: the text differs.
        assert_eq!(tree.key(&["-O2", "-DUNUSED=1"]).parts, base.parts, "an unused macro changes nothing without debug information");
        assert_ne!(tree.key(&["-O2", "-Dfile=renamed"]).parts.text, base.parts.text);
        // With -g3 the macro is in the object, used or not.
        assert_ne!(tree.key(&["-g3", "-DUNUSED=1"]).parts.text, tree.key(&["-g3", "-DUNUSED=2"]).parts.text);

        // Every byte of every file read: a comment, the spacing, the code.
        for (what, header) in [
            ("a comment", "#define ANSWER 42 /* still the answer */\n"),
            ("spacing", "#define  ANSWER  42 /* the answer */\n"),
            ("text the preprocessor skips", "#if 0\nnothing\n#endif\n#define ANSWER 42 /* the answer */\n"),
        ] {
            std::fs::write(tree.header(), header).unwrap();
            let edited = tree.key(&["-O2"]);
            assert_ne!(edited.parts.files, base.parts.files, "{what}");
            assert!(!base.still_holds(), "{what}: what was keyed before the edit is no longer there");
        }
        std::fs::write(tree.header(), HEADER).unwrap();
        std::fs::write(tree.source(), SOURCE.replace("int answer", "int   answer")).unwrap();
        let respaced = tree.key(&["-O2"]);
        assert_eq!(respaced.parts.text, base.parts.text, "the preprocessor does not show spacing");
        assert_ne!(respaced.parts.files, base.parts.files, "the file's bytes do");
        // Put back, everything keys as it did: old entries stay useful.
        std::fs::write(tree.source(), SOURCE).unwrap();
        assert_eq!(tree.key(&["-O2"]).parts, base.parts);

        // Another machine keys differently, with nothing else changed.
        let elsewhere = BuildConf { machine: "saturn".into(), ..tree.conf.clone() };
        let there = key(&elsewhere, &tree.cc, &tree.argv("gcc", &["-O2"]), false).unwrap();
        assert_ne!(there.parts.platform, base.parts.platform);
        assert_eq!(there.parts.text, base.parts.text);
    }

    #[test]
    fn the_definitions_of_macros_a_unit_never_uses_are_not_keyed() {
        if !have("gcc", Family::Gcc) {
            return;
        }
        // As Cactus's `cctk_DefineThorn.h`: one line per thorn of the
        // configuration, which nearly every unit includes and none uses.
        let tree = Tree::new();
        let with = |thorns: &str| format!("#define ANSWER 42 /* the answer */\n{thorns}");
        std::fs::write(tree.header(), with("#define THORN_A\n#define THORN_B\n")).unwrap();
        let base = tree.key(&["-O2"]);
        std::fs::write(tree.header(), with("#define THORN_A\n#define THORN_C 1\n")).unwrap();
        assert_eq!(tree.key(&["-O2"]).parts, base.parts, "another thorn's line");
        // A line broken with a backslash is one line.
        std::fs::write(tree.header(), with("#define THORN_A\n# define THORN_D \\\n  2\n")).unwrap();
        assert_eq!(tree.key(&["-O2"]).parts, base.parts, "a definition over two lines");
        // A line less: where what is compiled stands is in the text, and
        // where definitions stand only in messages (decision 29).
        std::fs::write(tree.header(), with("#define THORN_A\n")).unwrap();
        let fewer = tree.key(&["-O2"]);
        assert_eq!(fewer.parts, base.parts, "one line fewer");
        assert_ne!(fewer.whole(), base.whole(), "the files whole are not the same");
        // What stays: a macro the text tests, one defined twice (a warning
        // that it is redefined), one the command line gives, a reserved name.
        for (what, thorns, flags) in [
            ("tested", "#ifdef THORN_B\n#endif\n#define THORN_A\n#define THORN_C 1\n", &["-O2"][..]),
            ("defined twice", "#define THORN_A\n#define THORN_A 2\n", &["-O2"]),
            ("given on the command line", "#define THORN_A\n#define THORN_C 1\n", &["-O2", "-DTHORN_C=1"]),
            ("reserved", "#define THORN_A\n#define __THORN_C\n", &["-O2"]),
        ] {
            std::fs::write(tree.header(), with(thorns)).unwrap();
            let one = tree.key(flags);
            std::fs::write(tree.header(), with(&thorns.replace("THORN_A", "THORN_A_ELSE").replace("THORN_C 1", "THORN_C 9").replace("__THORN_C", "__THORN_C 9"))).unwrap();
            let other = tree.key(flags);
            match what {
                // THORN_A is never used: renaming it changes nothing.
                "tested" => assert_eq!(one.parts.files, other.parts.files, "{what}"),
                _ => assert_ne!(one.parts.files, other.parts.files, "{what}"),
            }
        }
        // A used macro's definition is keyed whole, comment and all.
        std::fs::write(tree.header(), "#define ANSWER 42 /* another comment */\n#define THORN_A\n#define THORN_B\n").unwrap();
        assert_ne!(tree.key(&["-O2"]).parts.files, base.parts.files);
        // With -g3 the object has every macro: whole files.
        std::fs::write(tree.header(), with("#define THORN_A\n#define THORN_B\n")).unwrap();
        let debug = tree.key(&["-g3"]);
        std::fs::write(tree.header(), with("#define THORN_A\n#define THORN_C 1\n")).unwrap();
        assert_ne!(tree.key(&["-g3"]).parts.files, debug.parts.files);
        std::fs::write(tree.header(), HEADER).unwrap();
    }

    #[test]
    fn a_header_with_code_keeps_its_definitions_in_the_key() {
        if !have("gcc", Family::Gcc) {
            return;
        }
        // GCC's `-Wmisleading-indentation` reads the raw lines between an
        // `if`'s body and what follows, a definition there too (decision
        // 30): a header with code is keyed whole.
        let tree = Tree::new();
        let with = |line: &str| format!("#define ANSWER 42 /* the answer */\nstatic inline int f(int a)\n{{\n  if (a)\n    a++;\n{line}\n    a--;\n  return a;\n}}\n");
        std::fs::write(tree.header(), with("#define UNUSED_X 1")).unwrap();
        let one = tree.key(&["-O2", "-Wall"]);
        std::fs::write(tree.header(), with("    #define UNUSED_X 1")).unwrap();
        assert_ne!(tree.key(&["-O2", "-Wall"]).parts.files, one.parts.files);
        std::fs::write(tree.header(), HEADER).unwrap();
    }

    #[test]
    fn with_exact_warnings_the_definitions_stay_in_the_key() {
        if !have("gcc", Family::Gcc) {
            return;
        }
        let tree = Tree::new();
        let exact = BuildConf { exact_warnings: true, ..tree.conf.clone() };
        let keyed = |conf: &BuildConf| key(conf, &tree.cc, &tree.argv("gcc", &["-O2"]), false).unwrap().parts;
        std::fs::write(tree.header(), "#define ANSWER 42\n#define THORN_A\n").unwrap();
        let (narrow, whole) = (keyed(&tree.conf), keyed(&exact));
        std::fs::write(tree.header(), "#define ANSWER 42\n#define THORN_B 1\n").unwrap();
        assert_eq!(keyed(&tree.conf), narrow, "narrowed by default");
        assert_ne!(keyed(&exact).files, whole.files, "whole with the knob");
        std::fs::write(tree.header(), HEADER).unwrap();
    }

    #[test]
    fn what_decides_a_warning_or_a_failure_is_keyed_or_refused() {
        if !have("gcc", Family::Gcc) {
            return;
        }
        let tree = Tree::new();
        // An include directory that is not there draws a warning.
        let missing = tree.cc.join("no-such-dir").display().to_string();
        let without = tree.key(&["-O2", "-I", &missing]).parts;
        std::fs::create_dir_all(&missing).unwrap();
        assert_ne!(tree.key(&["-O2", "-I", &missing]).parts.arguments, without.arguments);
        // `#pragma GCC dependency` compares modification times.
        std::fs::write(tree.header(), format!("{HEADER}#  pragma  GCC dependency \"t.h\"\n")).unwrap();
        assert!(key(&tree.conf, &tree.cc, &tree.argv("gcc", &["-O2"]), false).is_err_and(|why| why.contains("dependency")));
        std::fs::write(tree.header(), HEADER).unwrap();
        assert!(pragma_dependency(b"#pragma GCC dependency \"a\"\n") && pragma_dependency(b" # pragma GCC dependency x\n"));
        assert!(!pragma_dependency(b"#pragma GCC poison x\n") && !pragma_dependency(b"// #pragma GCC dependency\n"));
        // An include directory that is a file draws another.
        let file = tree.cc.join("a-file").display().to_string();
        std::fs::write(&file, "").unwrap();
        assert_ne!(tree.key(&["-O2", "-I", &file]).parts.arguments, without.arguments);
    }

    /// Whether the compile warns can turn on what no output shows; the
    /// key has what the preprocessor run said (review 6).
    #[test]
    fn what_the_preprocessor_says_is_keyed() {
        if !have("gcc", Family::Gcc) {
            return;
        }
        let tree = Tree::new();
        let plain = tree.key(&["-O2"]).parts;
        // `__has_include` deciding only a `#warning`.
        let local = tree.source().with_file_name("local_config.h");
        std::fs::write(tree.source(), format!("#if !__has_include(\"local_config.h\")\n#warning no local config\n#endif\n{SOURCE}")).unwrap();
        let warned = tree.key(&["-O2"]);
        std::fs::write(&local, "").unwrap();
        let mut quiet = tree.key(&["-O2"]);
        assert_ne!(warned.parts.text, quiet.parts.text, "__has_include's answer");
        assert!(warned.parts.text != plain.text && quiet.still_holds());
        std::fs::remove_file(&local).unwrap();
        // A `#pragma GCC dependency` as no line of a file spells it.
        let dep = tree.cc.join("dep.txt");
        std::fs::write(&dep, "").unwrap();
        std::fs::write(tree.source(), format!("_Pragma(\"GCC dependency \\\"{}\\\"\")\n{SOURCE}", dep.display())).unwrap();
        let set = |file: &Path, seconds: i64| std::fs::File::options().write(true).open(file).unwrap().set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds as u64)).unwrap();
        set(&tree.source(), 1_000_000_000);
        let older = tree.key(&["-O2"]).parts;
        set(&dep, 900_000_000);
        assert_ne!(tree.key(&["-O2"]).parts.text, older.text, "the source is older than the file it depends on, or not");
        // A name poisoned in the source and defined in a header the key
        // narrows: the warning names the source alone.
        std::fs::write(tree.source(), format!("{SOURCE}#pragma GCC poison XPOISON\n")).unwrap();
        std::fs::write(tree.header(), format!("{HEADER}#define XPOISON 1\n")).unwrap();
        let defined = tree.key(&["-O2"]).parts;
        let colored = tree.key(&["-O2", "-fdiagnostics-color=always"]).parts;
        std::fs::write(tree.header(), HEADER).unwrap();
        assert_ne!(tree.key(&["-O2"]).parts.text, defined.text, "poisoning a defined name");
        assert_ne!(tree.key(&["-O2", "-fdiagnostics-color=always"]).parts.text, colored.text, "in colors too");
        // A C unit with a `#pragma GCC diagnostic`, which GCC 14's `-E`
        // ignores for `-Wall`: a definition with `/*` in its comment draws
        // an error from the compile alone, so the files are not narrowed.
        std::fs::write(tree.source(), format!("#pragma GCC diagnostic error \"-Wall\"\n{SOURCE}")).unwrap();
        let quiet = tree.key(&["-O2"]).parts;
        std::fs::write(tree.header(), format!("{HEADER}#define THORN_IS_X 1 /* from arrangements/* */\n")).unwrap();
        assert_ne!(tree.key(&["-O2"]).parts.files, quiet.files, "narrowed under #pragma GCC diagnostic");
        std::fs::write(tree.header(), HEADER).unwrap();
        // And `#pragma GCC optimize`, after which the C compile defines
        // `__OPTIMIZE__` otherwise than `-E` does: SCALE is the compile's.
        std::fs::write(tree.source(), format!("#pragma GCC optimize(\"O0\")\n{SOURCE}int f(void) {{\n#ifdef __OPTIMIZE__\n  return 1;\n#else\n  return SCALE;\n#endif\n}}\n")).unwrap();
        std::fs::write(tree.header(), format!("{HEADER}#define SCALE 5\n")).unwrap();
        let five = tree.key(&["-O2"]).parts;
        std::fs::write(tree.header(), format!("{HEADER}#define SCALE 6\n")).unwrap();
        assert_ne!(tree.key(&["-O2"]).parts.files, five.files, "a macro only the compile uses");
        std::fs::write(tree.header(), HEADER).unwrap();
        std::fs::write(tree.source(), SOURCE).unwrap();
        // The run's `-v` lines are not what it said of the text.
        let said = b"Using built-in specs.\nignoring nonexistent directory \"/x\"\n/r/configs/c/build/T/a.c:2:2: warning: #warning hi [-Wcpp]\n";
        let map = tree.key(&["-O2"]).map.expect("a relocatable compile");
        assert_eq!(said_of_the_text(said, None), b"/r/configs/c/build/T/a.c:2:2: warning: #warning hi [-Wcpp]");
        assert!(said_of_the_text(&[b"In file included from ", tree.source().as_os_str().as_bytes(), b":1:\n"].concat(), Some(&map)).starts_with(b"In file included from @CACTUP_CONFIG@/"));
    }

    #[test]
    fn reads_diagnostics_in_colors_and_knows_where_e_ignores_pragmas() {
        let colored = b"\x1b[01m\x1b[Ka.c:2:9:\x1b[m\x1b[K \x1b[01;35m\x1b[Kwarning: \x1b[m\x1b[Kpoisoning existing macro \xe2\x80\x98\x1b[01m\x1b[KX\x1b[m\x1b[K\xe2\x80\x99 [\x1b]8;;https://gcc.gnu.org/x\x1b\\-Wfoo\x1b]8;;\x1b\\]\n";
        assert_eq!(plain(colored), "a.c:2:9: warning: poisoning existing macro \u{2018}X\u{2019} [-Wfoo]\n");
        assert_eq!(said_of_the_text(colored, None), b"a.c:2:9: warning: poisoning existing macro 'X' [-Wfoo]");
        use compile::Language;
        assert!(said_as_compiled(Language::C, false) && !said_as_compiled(Language::C, true));
        assert!(said_as_compiled(Language::Cxx, true));
        for line in ["#pragma GCC diagnostic error \"-Wall\"\n", "#pragma GCC optimize(\"O0\")\n", "#pragma GCC target(\"avx2\")\n", "#pragma GCC push_options\n", "#pragma GCC\n"] {
            assert!(acted_on_by_the_front_end(line.as_bytes()), "{line}");
        }
        for line in ["#pragma GCC system_header\n", "#pragma GCC poison X\n", "#pragma omp parallel\n", "int x;\n", "#pragma GCC unroll 4\n"] {
            assert!(!acted_on_by_the_front_end(line.as_bytes()), "{line}");
        }
    }

    #[test]
    fn exact_warnings_key_the_knob_and_the_locale() {
        if !have("gcc", Family::Gcc) {
            return;
        }
        let tree = Tree::new();
        let mut exact = tree.conf.clone();
        exact.exact_warnings = true;
        let keyed = |conf: &BuildConf| key(conf, &tree.cc, &tree.argv("gcc", &["-O2"]), false).unwrap().parts;
        let (exact, default) = (keyed(&exact), keyed(&tree.conf));
        assert_ne!(exact.environment, default.environment);
        // And the knob itself: a header with nothing to leave out is the
        // same bytes either way.
        assert_ne!(exact.arguments, default.arguments);
        assert!(String::from_utf8_lossy(&wording(Path::new("/usr/bin/gcc"), false).unwrap()).contains("unused variable"));
    }

    #[test]
    fn a_warning_in_the_source_leaves_the_headers_narrowed() {
        if !have("gcc", Family::Gcc) {
            return;
        }
        // CarpetX's `#warning "TODO"`: about the source, not the header of
        // definitions, which is narrowed all the same.
        let tree = Tree::new();
        std::fs::write(tree.source(), format!("{SOURCE}#warning \"TODO\"\n")).unwrap();
        std::fs::write(tree.header(), "#define ANSWER 42\n#define THORN_A\n").unwrap();
        let one = tree.key(&["-O2"]);
        std::fs::write(tree.header(), "#define ANSWER 42\n#define THORN_B 1\n").unwrap();
        assert_eq!(tree.key(&["-O2"]).parts, one.parts);
        std::fs::write(tree.source(), SOURCE).unwrap();
        std::fs::write(tree.header(), HEADER).unwrap();
    }

    #[test]
    fn reads_which_files_diagnostics_name() {
        let tmp = tempfile::tempdir().unwrap();
        let w = tmp.path().display();
        for name in ["a.c", "t.h", "u.h"] {
            std::fs::write(tmp.path().join(name), "").unwrap();
        }
        let said = format!("Using built-in specs.\nIn file included from {w}/a.c:1:\n{w}/t.h:3:9: warning: \"X\" redefined\n {w}/u.h:1: note: this is\n{w}/a.c: In function 'f':\n");
        let files = diagnosed_files(said.as_bytes()).unwrap();
        assert_eq!(files, ["a.c", "t.h", "u.h"].map(|name| tmp.path().join(name)).into_iter().collect());
        assert_eq!(diagnosed_files(b"gcc version 14\n").unwrap(), BTreeSet::new());
        // A name a `#line` gave, of no file: everything whole.
        assert!(diagnosed_files(format!("{w}/gen/thorns.ccl:3:2: warning: \"/*\" within comment [-Wcomment]\n").as_bytes()).is_none());
        // No file named: everything whole.
        assert!(diagnosed_files(b"<command-line>: warning: \"X\" redefined\n").is_none());
        assert!(diagnosed_files(b"cc1: warning: something about the command line\n").is_none());
    }

    #[test]
    fn a_definition_the_preprocessor_warns_of_stays_in_the_key() {
        if !have("gcc", Family::Gcc) {
            return;
        }
        // Its warning is the compile's message too: the files whole.
        let tree = Tree::new();
        let with = |thorns: &str| format!("#define ANSWER 42 /* the answer */\n{thorns}");
        for (quiet, warned) in [("#define NOTE dont\n", "#define NOTE don't\n"), ("#define NOTE 1\n", "#define unix 2\n")] {
            std::fs::write(tree.header(), with(quiet)).unwrap();
            let one = tree.key(&["-O2"]);
            std::fs::write(tree.header(), with(warned)).unwrap();
            assert_ne!(tree.key(&["-O2"]).parts.files, one.parts.files, "{warned}");
        }
        std::fs::write(tree.header(), HEADER).unwrap();
    }

    #[test]
    fn a_definition_in_a_comment_or_a_raw_string_is_none() {
        let names = |text: &str| definitions(text.as_bytes()).map(|name| String::from_utf8_lossy(name).into_owned()).collect::<Vec<_>>();
        assert_eq!(names("#define A\n/* old\n#define B 1 */ int x;\n#define C\n"), ["A", "C"]);
        assert_eq!(names("const char *s = R\"x(\n#define D\n)x\";\n#define E\n"), ["E"]);
        assert_eq!(names("// #define F\nint a = '\"'; /* \"*/\n#define G\n"), ["G"]);
        assert_eq!(names("#define H \"/*\"\n#define I\n"), ["H", "I"]);
        // A digit separator is no character constant: the comment after it
        // is one.
        assert_eq!(names("constexpr int K = 1'024; /* old\n#define J 1 */ int y;\n#define K\n"), ["K"]);
        assert_eq!(names("int a = 0xff'ff, b = u8'x'; /* c\n#define L */\n"), Vec::<String>::new());
        assert_eq!(names("char c = 'x'; /* c\n#define M */\n"), Vec::<String>::new());
        // What is left out is the definition alone; code beside a comment's
        // end stays.
        let kept = without_definitions(b"/* a\n#define B 1 */ int x;\n#define C\n", |_| true);
        assert_eq!(kept, b"/* a\n#define B 1 */ int x;\n");
    }

    #[test]
    fn a_compile_is_held_to_what_its_key_read() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::write(dir.join("a.h"), "").unwrap();
        std::os::unix::fs::symlink(dir.join("a.h"), dir.join("b.h")).unwrap();
        let keyed: BTreeSet<PathBuf> = [dir.join("a.h"), dir.join("s.c")].into();
        assert!(reads_match(&[dir.join("s.c"), dir.join("a.h")], &keyed));
        // Another name for the same file.
        std::fs::write(dir.join("s.c"), "").unwrap();
        assert!(reads_match(&[dir.join("s.c"), dir.join("b.h")], &keyed));
        // A file the compile did not read, or read besides.
        assert!(!reads_match(&[dir.join("s.c")], &keyed));
        assert!(!reads_match(&[dir.join("s.c"), dir.join("a.h"), dir.join("c.h")], &keyed));
        // `-MP`'s rules of no prerequisites name nothing new.
        let said = b"s.o: s.c a.h \\\n b.h\n\na.h:\n\nb.h:\n";
        assert_eq!(depend_files(said).unwrap(), ["s.c", "a.h", "b.h"].map(PathBuf::from));
        assert!(depend_files(b"s.o: s.c\nt.o: t.c\n").is_none());
    }

    #[test]
    fn reads_definitions_as_the_preprocessor_does() {
        assert_eq!(definition(b"#define A 1\n"), Some(&b"A"[..]));
        assert_eq!(definition(b"  #  undef  B_2\n"), Some(&b"B_2"[..]));
        assert_eq!(definition(b"#define F(x) x\n"), Some(&b"F"[..]));
        for not in [&b"#defineX\n"[..], b"#include <a.h>\n", b"int a; #define A\n", b"#define 1A\n", b"// #define A\n"] {
            assert_eq!(definition(not), None, "{}", String::from_utf8_lossy(not));
        }
        let lines: Vec<&[u8]> = logical_lines(b"a\n#define B \\\n 1\nc").collect();
        assert_eq!(lines, [&b"a\n"[..], b"#define B \\\n 1\n", b"c"]);
        assert_eq!(without_definitions(b"#define A \\\n 1\nint x;\n#define B\n", |name| name == b"A"), b"int x;\n#define B\n");
    }

    #[test]
    fn a_directive_behind_a_comment_is_still_a_directive() {
        if !have("gcc", Family::Gcc) {
            return;
        }
        let tree = Tree::new();
        std::fs::write(tree.source(), "/* config */ #include \"t.h\"\nint answer = ANSWER;\n").unwrap();
        let one = tree.key(&["-O2"]);
        std::fs::write(tree.header(), "#define ANSWER 43\n").unwrap();
        let other = tree.key(&["-O2"]);
        assert_ne!(one.parts.text, other.parts.text);
        assert_ne!(one.parts.files, other.parts.files);
    }

    #[test]
    fn with_debug_information_the_compile_directory_is_keyed() {
        if !have("gcc", Family::Gcc) {
            return;
        }
        // The working directory is the test process's, the same for both:
        // what differs is `$PWD`, which a compiler prefers when it names the
        // same place. It cannot be set for this process alone, so the two
        // keys are computed with and without debug information instead.
        let tree = Tree::new();
        let plain = tree.key(&["-O2"]);
        let debug = tree.key(&["-O2", "-g"]);
        assert_ne!(plain.parts.arguments, debug.parts.arguments);
    }

    #[test]
    fn what_it_cannot_follow_is_not_keyed() {
        if !have("gcc", Family::Gcc) {
            return;
        }
        let tree = Tree::new();
        assert!(tree.why_not(&["-MD"]).contains("without -MF"));
        assert!(tree.why_not(&["-MM"]).contains("-MM is a flag"));
        assert!(tree.why_not(&["-fsanitize=address"]).contains("does not follow"));

        std::fs::write(tree.source(), "__asm__(\".incbin \\\"blob.bin\\\"\");\n").unwrap();
        assert!(tree.why_not(&[]).contains("makes the assembler read another file"), "{}", tree.why_not(&[]));

        // A precompiled header beside a header: GCC would compile from it.
        std::fs::write(tree.source(), SOURCE).unwrap();
        let gch = tree.header().with_extension("h.gch");
        let built = Command::new("gcc").arg("-O2").arg(tree.header()).arg("-o").arg(&gch).status().unwrap();
        assert!(built.success());
        assert!(tree.why_not(&["-O2"]).contains("a precompiled header would be used"), "{}", tree.why_not(&["-O2"]));
        std::fs::remove_file(&gch).unwrap();
        assert!(key(&tree.conf, &tree.cc, &tree.argv("gcc", &["-O2"]), false).is_ok());

        std::fs::remove_file(tree.source()).unwrap();
        assert!(tree.why_not(&[]).contains("the preprocessor failed"));
        assert!(key(&tree.conf, &tree.cc, &tree.argv("cat", &[]), false).unwrap_err().contains("is not a compiler cactup knows"));
    }

    #[test]
    fn clang_keys_what_it_can_and_declines_what_it_cannot() {
        if !have("clang", Family::Clang) {
            return;
        }
        let (here, there) = (Tree::new(), Tree::new());
        let clang = |tree: &Tree, flags: &[&str]| key(&tree.conf, &tree.cc, &tree.argv("clang", flags), false);
        let (a, b) = (clang(&here, &["-g", "-O2"]).unwrap(), clang(&there, &["-g", "-O2"]).unwrap());
        assert_eq!(a.parts, b.parts);
        // The same file run as C++ is another compiler.
        assert_ne!(key(&here.conf, &here.cc, &here.argv("clang++", &["-g", "-O2"]), false).unwrap().parts.compiler, a.parts.compiler);
        // With OpenMP, Clang records source paths no map reaches: the key
        // then knows where the tree is.
        let (a, b) = (clang(&here, &["-g", "-fopenmp"]).unwrap(), clang(&there, &["-g", "-fopenmp"]).unwrap());
        assert!(!a.relocatable());
        assert_ne!(a.parts.key(), b.parts.key());
        // A file forced in may be replaced by a precompiled one.
        let forced = clang(&here, &["-include", here.header().to_str().unwrap()]).unwrap_err();
        assert!(forced.contains("-include with Clang"), "{forced}");
    }

    /// What leads the driver elsewhere from another directory keeps the
    /// compile out of the cache (a hit could not tell).
    #[test]
    fn a_driver_led_by_relative_names_is_refused() {
        let env = |pairs: &'static [(&'static str, &'static str)]| move |variable: &str| pairs.iter().find(|(name, _)| *name == variable).map(|(_, value)| OsString::from(value));
        let (gfortran, gcc) = (OsStr::new("gfortran"), OsStr::new("gcc"));
        assert_eq!(driver_led_elsewhere(gfortran, true, env(&[("PATH", "/usr/bin:/bin"), ("LIBRARY_PATH", "/opt/lib")])), None);
        // A relative `LIBRARY_PATH` entry is watched, not refused (decision
        // 22), for Fortran too.
        assert_eq!(driver_led_elsewhere(gfortran, true, env(&[("LIBRARY_PATH", ":/opt/lib")])), None);
        assert_eq!(driver_led_elsewhere(gcc, false, env(&[("LIBRARY_PATH", ":/opt/lib")])), None);
        for compiler in [(gfortran, true), (gcc, false)] {
            assert!(driver_led_elsewhere(compiler.0, compiler.1, env(&[("COMPILER_PATH", "/opt:lib")])).is_some());
            assert!(driver_led_elsewhere(compiler.0, compiler.1, env(&[("GCC_EXEC_PREFIX", "x86/")])).is_some());
            assert!(driver_led_elsewhere(compiler.0, compiler.1, env(&[("LD_PRELOAD", "./libx.so")])).is_some());
            assert!(driver_led_elsewhere(compiler.0, compiler.1, env(&[("LD_AUDIT", "/opt/libaudit.so")])).is_some());
            assert_eq!(driver_led_elsewhere(compiler.0, compiler.1, env(&[("LD_PRELOAD", "libx.so /opt/liby.so")])), None);
        }
        // `PATH` and the loader's search path are judged by where the
        // searches go (`identity`), but for a Fortran driver named without
        // a `/`, which finds itself along `PATH` from another directory.
        assert!(driver_led_elsewhere(gfortran, true, env(&[("PATH", ".:/usr/bin")])).is_some_and(|why| why.starts_with("PATH")));
        // After the driver's own place, it is never walked.
        if Path::new("/usr/bin/gfortran").is_file() {
            assert_eq!(driver_led_elsewhere(gfortran, true, env(&[("PATH", "/usr/bin:.")])), None);
        }
        assert_eq!(driver_led_elsewhere(OsStr::new("/usr/bin/gfortran"), true, env(&[("PATH", "/usr/bin:.")])), None);
        assert_eq!(driver_led_elsewhere(gcc, false, env(&[("PATH", "/usr/bin:."), ("LD_LIBRARY_PATH", "/opt/lib:")])), None);
        assert!(driver_led_elsewhere(OsStr::new("../bin/gfortran"), true, env(&[])).is_some());
        // A name that is not UTF-8 cannot be remembered with the identity.
        let odd = |variable: &str| (variable == "PATH").then(|| OsString::from(OsStr::from_bytes(b"/usr/bin:/opt/\xff/bin")));
        assert!(driver_led_elsewhere(gcc, false, odd).is_some_and(|why| why.contains("not UTF-8")));
        assert_eq!(driver_led_elsewhere(OsStr::new("../bin/gcc"), false, env(&[])), None);
    }
}
