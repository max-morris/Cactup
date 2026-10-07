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
use super::hash::{file_digest, Hasher};
use super::identity::{self, Compiler, Family};
use super::{environment, platform, BuildConf};
use crate::Res;
use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, ErrorKind, Read as _};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};

/// What the Cactus root is called in a key, and in a mapped compile's
/// `__FILE__`: sources then read `./arrangements/<Arrangement>/<Thorn>/src/…`.
const ROOT_NAME: &str = "./";
/// What the configuration directory is called, whatever its name.
const CONFIG_NAME: &str = "./configs/@config/";

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
        // Only where a path begins: `/x/w/Cactus/` is not `/w/Cactus/`.
        let in_a_path = |byte: u8| byte.is_ascii_alphanumeric() || b"._-+~@/".contains(&byte);
        let mut out = Vec::with_capacity(text.len());
        let mut at = 0;
        while at < text.len() {
            let begins = at == 0 || !in_a_path(text[at - 1]);
            match self.from_to.iter().find(|(from, _)| begins && text[at..].starts_with(from)) {
                Some((from, to)) => {
                    out.extend_from_slice(if *to == CONFIG_NAME { CONFIG_TOKEN } else { ROOT_TOKEN });
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
pub const KEY_LABEL: &str = "key-5";

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
    map: Option<PathMap>,
    /// The dependency file the key's preprocessor run wrote, under a
    /// temporary name, and the name the compile would give it (§18.8).
    depend: Option<(tempfile::TempPath, PathBuf)>,
}

/// Key the compile `argv` asks for (the compiler's words, then its
/// arguments). `Err` says why it is not one the cache keys; the compile
/// itself is none of this function's business.
///
/// With `depend`, a dependency file the compile asks for is written by the
/// key's preprocessor run, under a temporary name ([`Keyed::keep_depend`]
/// gives it the compile's): what a hit needs, since no compile runs.
pub fn key(conf: &BuildConf, cc_dir: &Path, argv: &[OsString], depend: bool) -> Result<Keyed, String> {
    let whole = |e: anyhow::Error| format!("{e:#}");
    // Before anything else: a variable that rules the compile out says so
    // whatever the compiler.
    environment::digest(true)?;
    // The compiler before its arguments: for a compiler the cache has no
    // reader for, "which compiler" is the reason worth giving, not whichever
    // of its flags the GCC reader trips over first.
    let compiler = identity::identify(cc_dir, &argv[0]).map_err(whole)?;
    let compile = compile::parse(&argv[1..])?;
    // The locale only where the compiler's trial says it can matter, and
    // wherever the compile converts character sets: the trial does not, and
    // a conversion such as `ASCII//TRANSLIT` follows the locale (§18.8).
    let environment = environment::digest(!compiler.locale_neutral || compile.charset)?;
    if compiler.family == Family::Clang && compile.forced_include {
        // Clang takes `<file>.pch` or `<file>.gch` in place of a file given
        // with `-include`, and its `-E` does not say so.
        return Err("-include with Clang may bring in a precompiled header, which the cache does not follow".to_owned());
    }
    let platform = platform::platform(conf, cc_dir).map_err(whole)?;
    if compile.native && platform.mixed {
        // Seen for real: one compile of one file, run twice, two objects.
        return Err("it targets the processor it runs on (native), and this host's processors are not all alike".to_owned());
    }
    let map = PathMap::new(conf, &compiler, &compile);
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
    // not).
    if compile.debug {
        let cwd = std::env::current_dir().context("Failed to read the working directory").map_err(whole)?;
        arguments.feed(b"cwd");
        arguments.feed(&mapped(cwd.as_os_str().as_bytes()));
        arguments.feed(&mapped(std::env::var_os("PWD").unwrap_or_default().as_bytes()));
    }

    let name = argv[0].clone();
    let depend = match depend {
        true => depend_flags(&compile).map_err(whole)?,
        false => None,
    };
    let read = preprocess(&compiler, &name, &compile, map.as_ref(), depend.as_ref().map(|(_, flags)| flags.as_slice()))
        .map_err(whole)?;
    let depend = depend.map(|(files, _)| files);
    let parts = Parts {
        platform: platform.digest,
        compiler: compiler.id.clone(),
        arguments: arguments.hex(),
        environment,
        text: read.text,
        files: read.files,
    };
    Ok(Keyed { compile, compiler, parts, text_bytes: read.text_bytes, files: read.count, name, map, depend })
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
    let named = |flag: &str| compile.depend.iter().any(|arg| arg == flag);
    // The last `-MF` is the one the compiler writes.
    let at = compile.depend.iter().rposition(|arg| arg == "-MF").context("a dependency file without -MF")?;
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
    pub fn still_holds(&self) -> bool {
        preprocess(&self.compiler, &self.name, &self.compile, self.map.as_ref(), None)
            .is_ok_and(|read| read.text == self.parts.text && read.files == self.parts.files)
    }

    /// The flags that make the compile record its paths as the key does:
    /// the path map's, if the key was made with it.
    pub fn compile_flags(&self) -> Vec<OsString> {
        self.map.as_ref().map(PathMap::flags).unwrap_or_default()
    }

    /// The map the key was made with.
    pub fn map(&self) -> Option<&PathMap> {
        self.map.as_ref()
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
    /// Digest of the contents of the files it named.
    files: String,
    count: usize,
}

/// A line that names a file: a line marker of preprocessor output
/// (`# 12 "dir/file.h" 1`), or a line directive of a source (`#line 12
/// "file.c"`).
struct Naming<'a> {
    /// What stands before the file name, the opening quote included.
    head: &'a [u8],
    /// The name, with its escapes undone.
    name: Vec<u8>,
    /// What follows the name, from the closing quote on.
    tail: &'a [u8],
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
fn naming<'a>(line: &'a [u8], opening: &[u8]) -> Result<Option<Naming<'a>>, String> {
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
fn content_digest(path: &Path, map: Option<&PathMap>) -> Res<String> {
    let Some(map) = map else { return file_digest(path) };
    let bytes = std::fs::read(path).with_context(|| format!("Failed to read {}", path.display()))?;
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
        None => hasher.feed(&bytes),
    }
    Ok(hasher.hex())
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
fn preprocess(compiler: &Compiler, name: &OsStr, compile: &Compile, map: Option<&PathMap>, depend: Option<&[OsString]>) -> Res<Read> {
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
    let (text, text_bytes, mut named) = read?;
    let status = status.context("Failed to wait for the preprocessor")?;
    if !status.success() {
        bail!("the preprocessor failed ({status})");
    }
    flags_from_elsewhere(compiler.family, compiler.specs.as_deref(), &said).map_err(anyhow::Error::msg)?;
    // The source itself, whatever the markers call it.
    let source = compile.source.as_os_str().as_bytes();
    named.insert((map.map_or_else(|| source.to_vec(), |map| map.apply(source)), compile.source.clone()), true);

    // The files, each under its mapped name. A file the preprocessor
    // entered, it opened: if that cannot be read here, its name was misread
    // or it is gone, and there is no key. A name that only a `#line` gave
    // (generated code names its origin so) may be of no file at all; the
    // bytes compiled are those of the file the directive stands in, which
    // was entered.
    let mut files = Hasher::new("files");
    for ((mapped, path), entered) in &named {
        files.feed(mapped);
        // "Not there" is the one thing a made-up name may be: any other
        // failure to read is a failure to key.
        let absent = || matches!(path.metadata(), Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory));
        match content_digest(path, map) {
            Ok(digest) => files.feed(digest.as_bytes()),
            Err(_) if !entered && absent() => files.feed(b"no such file"),
            Err(e) => bail!("a file the compile reads cannot be read to key it ({e:#})"),
        }
    }
    Ok(Read { text, text_bytes, files: files.hex(), count: named.len() })
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
fn flags_from_elsewhere(family: Family, specs: Option<&Path>, said: &[u8]) -> Result<(), String> {
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
        Family::Gcc => {
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
type Named = BTreeMap<(Vec<u8>, PathBuf), bool>;

/// Digest the preprocessor's output as it comes, and collect the files it
/// names.
fn digest_output(child: &mut std::process::Child, map: Option<&PathMap>) -> Res<(String, u64, Named)> {
    let mut output = BufReader::new(child.stdout.take().context("the preprocessor has no output")?);
    let mut hasher = Hasher::new("text");
    let mut named = Named::new();
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
            None => feed(&line),
        }
    }
    hasher.end_stream(bytes);
    Ok((hasher.hex(), bytes, named))
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
    fn maps_names_as_the_compiler_does() {
        let map = map(&[("/w/Cactus/", ROOT_NAME), ("/w/Cactus/configs/sim/", CONFIG_NAME)]);
        assert_eq!(applied(&map, "/w/Cactus/arrangements/A/T/src/a.c"), "./arrangements/A/T/src/a.c");
        assert_eq!(applied(&map, "/w/Cactus/configs/sim/build/T/a.c"), "./configs/@config/build/T/a.c");
        // Another configuration is under the root, not this configuration.
        assert_eq!(applied(&map, "/w/Cactus/configs/simple/x"), "./configs/simple/x");
        // A prefix of the name, and only at its start; a directory that
        // merely begins alike is another directory.
        for untouched in ["/w/Cactus-libs/include/x.h", "/w/Cactus", "/opt/w/Cactus/x.h", "x/w/Cactus/y", "relative.h"] {
            assert_eq!(applied(&map, untouched), untouched);
        }
        // The flags name the less specific directory first.
        let flags: Vec<String> = map.flags().into_iter().map(|f| f.into_string().unwrap()).collect();
        assert_eq!(flags, ["-ffile-prefix-map=/w/Cactus/=./", "-ffile-prefix-map=/w/Cactus/configs/sim/=./configs/@config/"]);
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
            content_digest(&file, map).unwrap()
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
            let compiler = Compiler { path: PathBuf::from("/usr/bin/cc"), family, relocates: true, locale_neutral: true, id: String::new(), specs: None };
            let command = preprocessor(&compiler, OsStr::new("cc"), &compile, None, None);
            let given: Vec<&OsStr> = command.get_args().collect();
            assert!(given.contains(&OsStr::new("-E")) && given.contains(&OsStr::new("/c/build/T/a.c")), "{given:?}");
            assert!(!given.iter().any(|arg| arg.as_bytes().starts_with(b"-M") || depend.contains(&arg.to_str().unwrap())), "{given:?}");
        }
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
        assert_eq!(parts.key(), "f8ac93a68a8d8e316472e78ee11e6d694384a157e89aac8d14e2465bacdc781c");
        assert_eq!(PathMap::description(), ["-ffile-prefix-map", "./", "./configs/@config/"]);
    }

    /// Messages keep the map's directories as tokens where a path begins,
    /// and only there, and come back as this build's.
    #[test]
    fn messages_travel_with_the_tree_they_name() {
        let map = map(&[("/w/Cactus/configs/sim/", CONFIG_NAME), ("/w/Cactus/", ROOT_NAME)]);
        let said = b"/w/Cactus/arrangements/A/T/src/a.c:3: warning: x\nIn file included from /w/Cactus/configs/sim/bindings/h.h,\n/x/w/Cactus/y and '/w/Cactus/z'\n";
        let stored = map.messages_for_the_store(said);
        assert_eq!(
            String::from_utf8_lossy(&stored),
            "@CACTUP_ROOT@/arrangements/A/T/src/a.c:3: warning: x\nIn file included from @CACTUP_CONFIG@/bindings/h.h,\n/x/w/Cactus/y and '@CACTUP_ROOT@/z'\n"
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
        };
        assert_eq!(
            String::from_utf8_lossy(&messages_for_this_build(&conf, &stored)),
            "/v/Cactus/arrangements/A/T/src/a.c:3: warning: x\nIn file included from /v/Cactus/configs/other/bindings/h.h,\n/x/w/Cactus/y and '/v/Cactus/z'\n"
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

        fn key(&self, flags: &[&str]) -> Keyed {
            key(&self.conf, &self.cc, &self.argv("gcc", flags), false).unwrap()
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
        let a = here.key(&["-O2", "-g"]);
        assert!(a.relocatable() && a.files >= 2, "the source and its header, and what the compiler includes by itself");
        assert_eq!(there.key(&["-O2", "-g"]).parts, a.parts, "nothing in the key may know where the tree is");
        assert_eq!(renamed.key(&["-O2", "-g"]).parts, a.parts, "nor what the configuration is called");
        assert!(a.still_holds());
    }

    #[test]
    fn what_changes_the_object_changes_the_key() {
        if !have("gcc", Family::Gcc) {
            return;
        }
        let tree = Tree::new();
        let base = tree.key(&["-O2"]);

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
}
