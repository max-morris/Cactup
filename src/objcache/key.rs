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
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};

/// What the Cactus root is called in a key, and in a mapped compile's
/// `__FILE__`: sources then read `./arrangements/<Arrangement>/<Thorn>/src/…`.
const ROOT_NAME: &str = "./";
/// What the configuration directory is called, whatever its name.
const CONFIG_NAME: &str = "./configs/@config/";

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
        if !compiler.relocates || unmapped_paths {
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
        let flag = |(from, to): &(Vec<u8>, &str)| {
            let mut flag = OsString::from("-ffile-prefix-map=");
            flag.push(OsStr::from_bytes(from));
            flag.push(format!("={to}"));
            flag
        };
        self.from_to.iter().rev().map(flag).collect()
    }

    /// The file name `name` as a mapped compile records it.
    pub fn apply(&self, name: &[u8]) -> Vec<u8> {
        match self.from_to.iter().find_map(|(from, to)| Some((to, name.strip_prefix(from.as_slice())?))) {
            Some((to, rest)) => [to.as_bytes(), rest].concat(),
            None => name.to_vec(),
        }
    }

    /// An argument with the file name in it mapped: the whole argument, or
    /// what follows its `=` (`--sysroot=<dir>`).
    fn apply_to_argument(&self, argument: &[u8]) -> Vec<u8> {
        match argument.iter().position(|b| *b == b'=') {
            Some(at) if !argument.starts_with(b"/") => [&argument[..=at], self.apply(&argument[at + 1..]).as_slice()].concat(),
            _ => self.apply(argument),
        }
    }
}

/// The digests a key is made of. Kept apart in the event log, so that two
/// builds can be compared part by part: which part differed is why a
/// compile would have missed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

impl Parts {
    pub fn key(&self) -> String {
        let mut hasher = Hasher::new("key-2");
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
}

/// Key the compile `argv` asks for (the compiler's words, then its
/// arguments). `Err` says why it is not one the cache keys; the compile
/// itself is none of this function's business.
pub fn key(conf: &BuildConf, cc_dir: &Path, argv: &[OsString]) -> Result<Keyed, String> {
    let whole = |e: anyhow::Error| format!("{e:#}");
    let environment = environment::digest()?;
    // The compiler before its arguments: for a compiler the cache has no
    // reader for, "which compiler" is the reason worth giving, not whichever
    // of its flags the GCC reader trips over first.
    let compiler = identity::identify(cc_dir, &argv[0]).map_err(whole)?;
    let compile = compile::parse(&argv[1..])?;
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
    arguments.feed(if map.is_some() { b"mapped" } else { b"unmapped" });
    for argument in &compile.keyed {
        match &map {
            Some(map) => arguments.feed(&map.apply_to_argument(argument.as_bytes())),
            None => arguments.feed(argument.as_bytes()),
        }
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
    let read = preprocess(&compiler, &name, &compile, map.as_ref()).map_err(whole)?;
    let parts = Parts {
        platform: platform.digest,
        compiler: compiler.id.clone(),
        arguments: arguments.hex(),
        environment,
        text: read.text,
        files: read.files,
    };
    Ok(Keyed { compile, compiler, parts, text_bytes: read.text_bytes, files: read.count, name, map })
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
        preprocess(&self.compiler, &self.name, &self.compile, self.map.as_ref())
            .is_ok_and(|read| read.text == self.parts.text && read.files == self.parts.files)
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

/// A line marker of preprocessor output, `# 12 "dir/file.h" 1`, taken
/// apart: what stands before the file name, the name with its escapes
/// undone, and what follows the closing quote.
fn line_marker(line: &[u8]) -> Option<(&[u8], Vec<u8>, &[u8])> {
    line_directive(line, b"# ")
}

/// A line that begins with `opening` (`# ` in preprocessor output, `#line `
/// in a source), then a line number and a quoted file name, taken apart as
/// for [`line_marker`].
fn line_directive<'a>(line: &'a [u8], opening: &[u8]) -> Option<(&'a [u8], Vec<u8>, &'a [u8])> {
    let rest = line.strip_prefix(opening)?;
    let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
    let quoted = rest[digits..].strip_prefix(b" \"")?;
    if digits == 0 {
        return None;
    }
    let head = &line[..line.len() - quoted.len()];
    let (mut name, mut bytes) = (Vec::new(), quoted.iter().enumerate());
    while let Some((at, byte)) = bytes.next() {
        match byte {
            b'"' => return Some((head, name, &quoted[at..])),
            b'\\' => name.push(*bytes.next()?.1),
            byte => name.push(*byte),
        }
    }
    None
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
    match line_directive(&bytes[..first], b"#line ") {
        Some((head, name, tail)) => {
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
fn preprocess(compiler: &Compiler, name: &OsStr, compile: &Compile, map: Option<&PathMap>) -> Res<Read> {
    let mut command = Command::new(&compiler.path);
    command.arg0(name).args(&compile.preprocess).arg("-E");
    if compile.macros_in_debug {
        command.arg("-dD");
    }
    if compiler.family == Family::Gcc {
        command.arg("-fpch-preprocess");
    }
    if let Some(map) = map {
        command.args(map.flags());
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("Failed to run {} as a preprocessor", compiler.path.display()))?;
    PREPROCESSOR.store(child.id() as i32, Ordering::SeqCst);
    let read = digest_output(&mut child, map);
    let status = child.wait();
    PREPROCESSOR.store(0, Ordering::SeqCst);
    let (text, text_bytes, named) = read?;
    let status = status.context("Failed to wait for the preprocessor")?;
    if !status.success() {
        bail!("the preprocessor failed ({status})");
    }

    // The files, each under its mapped name. One that cannot be read is one
    // a `#line` made up (generated code names its origin so); the
    // preprocessor read the file it stands in, which is named too.
    let mut files = Hasher::new("files");
    for (mapped, path) in &named {
        files.feed(mapped);
        match content_digest(path, map) {
            Ok(digest) => files.feed(digest.as_bytes()),
            Err(_) => files.feed(b"no such file"),
        }
    }
    Ok(Read { text, text_bytes, files: files.hex(), count: named.len() })
}

/// Digest the preprocessor's output as it comes, and collect the files it
/// names: each as its mapped name and its path. (Two files can share a
/// mapped name — `./x.h` in the working directory and `x.h` in the Cactus
/// root — and both are read.)
fn digest_output(child: &mut std::process::Child, map: Option<&PathMap>) -> Res<(String, u64, BTreeSet<(Vec<u8>, PathBuf)>)> {
    let mut output = BufReader::new(child.stdout.take().context("the preprocessor has no output")?);
    let mut hasher = Hasher::new("text");
    let mut named = BTreeSet::new();
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
        match line_marker(&line) {
            Some((head, name, tail)) => {
                let mapped = map.map_or_else(|| name.clone(), |map| map.apply(&name));
                feed(head);
                feed(&mapped);
                feed(tail);
                // `<built-in>`, `<command-line>`: not files. A name ending
                // in `//` is GCC's note of the working directory.
                let pseudo = name.starts_with(b"<") || name.ends_with(b"//") || name.is_empty();
                if !pseudo {
                    named.insert((mapped, PathBuf::from(OsString::from_vec(name))));
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
        // In an argument: the whole of it, or what follows `=`.
        let argument = |text: &str| String::from_utf8(map.apply_to_argument(text.as_bytes())).unwrap();
        assert_eq!(argument("/w/Cactus/x.h"), "./x.h");
        assert_eq!(argument("--sysroot=/w/Cactus/sys"), "--sysroot=./sys");
        assert_eq!(argument("-O2"), "-O2");
    }

    #[test]
    fn takes_line_markers_apart() {
        let marker = |line: &str| line_marker(line.as_bytes()).map(|(head, name, tail)| {
            (String::from_utf8_lossy(head).into_owned(), String::from_utf8(name).unwrap(), String::from_utf8_lossy(tail).into_owned())
        });
        assert_eq!(marker("# 12 \"a.h\" 1\n"), Some(("# 12 \"".into(), "a.h".into(), "\" 1\n".into())));
        assert_eq!(marker("# 0 \"<built-in>\"\n"), Some(("# 0 \"".into(), "<built-in>".into(), "\"\n".into())));
        // Escapes in a name are undone.
        assert_eq!(marker("# 1 \"a\\\\b\\\"c.h\"\n").unwrap().1, "a\\b\"c.h");
        for not in ["#define X 1\n", "#pragma once\n", "# pragma omp\n", "int x; # 1 \"a\"\n", "#\n", "", "# 1\n", "# x \"a.h\"\n", "# 1 \"open\n"] {
            assert_eq!(marker(not), None, "{not:?}");
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
            key(&self.conf, &self.cc, &self.argv("gcc", flags)).unwrap()
        }

        fn why_not(&self, flags: &[&str]) -> String {
            key(&self.conf, &self.cc, &self.argv("gcc", flags)).unwrap_err()
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
        let there = key(&elsewhere, &tree.cc, &tree.argv("gcc", &["-O2"])).unwrap();
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
        assert!(tree.why_not(&["-MD"]).contains("-MD is a flag"));
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
        assert!(key(&tree.conf, &tree.cc, &tree.argv("gcc", &["-O2"])).is_ok());

        std::fs::remove_file(tree.source()).unwrap();
        assert!(tree.why_not(&[]).contains("the preprocessor failed"));
        assert!(key(&tree.conf, &tree.cc, &tree.argv("cat", &[])).unwrap_err().contains("is not a compiler cactup knows"));
    }

    #[test]
    fn clang_keys_what_it_can_and_declines_what_it_cannot() {
        if !have("clang", Family::Clang) {
            return;
        }
        let (here, there) = (Tree::new(), Tree::new());
        let clang = |tree: &Tree, flags: &[&str]| key(&tree.conf, &tree.cc, &tree.argv("clang", flags));
        let (a, b) = (clang(&here, &["-g", "-O2"]).unwrap(), clang(&there, &["-g", "-O2"]).unwrap());
        assert_eq!(a.parts, b.parts);
        // The same file run as C++ is another compiler.
        assert_ne!(key(&here.conf, &here.cc, &here.argv("clang++", &["-g", "-O2"])).unwrap().parts.compiler, a.parts.compiler);
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
