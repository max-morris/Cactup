//! The key of one compile: a digest that two compiles share exactly when
//! they would produce the same object (§18.1 rule 1).
//!
//! It is the compiler's own view of its input that is keyed: the output of
//! the same compiler run with the same arguments and `-E`. Whatever a
//! source includes, however the include paths and macros are set, ends up
//! in that text, so nothing about headers has to be tracked or guessed.
//! Around it go the things the text cannot show: which compiler it is,
//! the arguments that decide code generation, the platform, and the part
//! of the environment compilers read.
//!
//! **Paths.** Cactus compiles with absolute paths, and an object records
//! them (`__FILE__` in every `CCTK_WARN`, the directory in debug
//! information), so as it stands an object belongs to one configuration of
//! one installation. For compilers that take `-ffile-prefix-map`, the key
//! is computed as if the compile ran with the Cactus root and the
//! configuration directory mapped to fixed names ([`PathMap`]) — which is
//! how a serving cache will run it. Nothing is added to the real compile
//! while the cache only records.

use super::compile::{self, Compile};
use super::hash::Hasher;
use super::identity::{self, Compiler, Family};
use super::{environment, platform, BuildConf};
use crate::Res;
use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;
use std::process::{Command, Stdio};

/// What the Cactus root is called in a key, and in a mapped compile's
/// `__FILE__`: sources then read `./arrangements/<Arrangement>/<Thorn>/src/…`.
const ROOT_NAME: &str = ".";
/// What the configuration directory is called, whatever its name.
const CONFIG_NAME: &str = "./configs/@config";

/// The directories a key must not depend on, with the names that stand for
/// them.
#[derive(Debug)]
pub struct PathMap {
    /// Longest first: the configuration directory lies inside the Cactus
    /// root, and must be recognized before it.
    from_to: Vec<(Vec<u8>, &'static str)>,
}

impl PathMap {
    /// The map for a build, if this compiler can be made to honor it.
    /// Both the spelling cactup has for each directory and its physical
    /// path are mapped: make reports one, a shell's `pwd` may report the
    /// other.
    pub fn new(conf: &BuildConf, compiler: &Compiler) -> Option<Self> {
        // `-ffile-prefix-map`: GCC 8, Clang 10.
        let minimum = match compiler.family {
            Family::Gcc => (8, 0),
            Family::Clang => (10, 0),
        };
        if compiler.version < minimum {
            return None;
        }
        let mut from_to = Vec::new();
        for (dir, name) in [(&conf.config_dir, CONFIG_NAME), (&conf.cactus_root, ROOT_NAME)] {
            for spelling in [Some(dir.clone()), std::fs::canonicalize(dir).ok()].into_iter().flatten() {
                let spelling = spelling.into_os_string().into_vec();
                if !from_to.iter().any(|(from, _)| *from == spelling) {
                    from_to.push((spelling, name));
                }
            }
        }
        from_to.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
        Some(Self { from_to })
    }

    /// The compiler flags that make a compile honor the map, for `__FILE__`
    /// and for debug information. Shortest directory first: of several
    /// maps that match, GCC and Clang both take the last given (checked on
    /// GCC 14.2 and Clang 19.1).
    pub fn flags(&self) -> Vec<OsString> {
        let flag = |(from, to): &(Vec<u8>, &str)| {
            let mut flag = OsString::from("-ffile-prefix-map=");
            flag.push(OsStr::from_bytes(from));
            flag.push(format!("={to}"));
            flag
        };
        self.from_to.iter().rev().map(flag).collect()
    }

    /// `text` with every mapped directory replaced by its name, wherever it
    /// stands as a whole path prefix (so `/a/Cactus` is not found in
    /// `/a/Cactus-old`).
    pub fn apply(&self, text: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(text.len());
        let mut rest = text;
        'text: while !rest.is_empty() {
            for (from, to) in &self.from_to {
                if let Some(after) = rest.strip_prefix(from.as_slice())
                    && matches!(after.first(), None | Some(b'/' | b'"' | b'=' | b':'))
                {
                    out.extend_from_slice(to.as_bytes());
                    rest = after;
                    continue 'text;
                }
            }
            out.push(rest[0]);
            rest = &rest[1..];
        }
        out
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
    pub text: String,
}

impl Parts {
    pub fn key(&self) -> String {
        let mut hasher = Hasher::new("key-1");
        for part in [&self.platform, &self.compiler, &self.arguments, &self.environment, &self.text] {
            hasher.feed(part.as_bytes());
        }
        hasher.hex()
    }
}

/// A compile the cache has a key for.
#[derive(Debug)]
pub struct Keyed {
    pub compile: Compile,
    pub parts: Parts,
    /// How much preprocessed text went into the key.
    pub text_bytes: u64,
    program: OsString,
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
    let platform = platform::digest(conf, cc_dir).map_err(whole)?;
    let map = PathMap::new(conf, &compiler);

    let mut arguments = Hasher::new("arguments");
    arguments.feed(compile.language.name().as_bytes());
    for argument in &compile.keyed {
        match &map {
            Some(map) => arguments.feed(&map.apply(argument.as_bytes())),
            None => arguments.feed(argument.as_bytes()),
        }
    }
    // Debug information records the directory the compiler ran in. Mapped,
    // that is the same name everywhere; unmapped, it is part of the object.
    if compile.debug && map.is_none() {
        let cwd = std::env::current_dir().context("Failed to read the working directory").map_err(whole)?;
        arguments.feed(b"cwd");
        arguments.feed(cwd.as_os_str().as_bytes());
    }

    let (text, text_bytes) = preprocess(&argv[0], &compile, map.as_ref()).map_err(whole)?;
    let parts = Parts { platform, compiler: compiler.id, arguments: arguments.hex(), environment, text };
    Ok(Keyed { compile, parts, text_bytes, program: argv[0].clone(), map })
}

impl Keyed {
    /// Is the preprocessed text still what was keyed? Run after the
    /// compile: a header edited while the compile ran makes an object of
    /// the new text under the key of the old.
    pub fn still_holds(&self) -> bool {
        preprocess(&self.program, &self.compile, self.map.as_ref()).is_ok_and(|(text, _)| text == self.parts.text)
    }
}

/// Is `line` a line marker of preprocessor output (`# 12 "file.h" 1`)?
fn is_line_marker(line: &[u8]) -> bool {
    line.strip_prefix(b"# ").is_some_and(|rest| rest.first().is_some_and(u8::is_ascii_digit))
}

/// Run the preprocessor for `compile` and digest what it prints: the same
/// program and arguments as the compile, with `-E` in place of `-c -o
/// <object>`. Comments are kept (`-C`), so that an edit to one is an edit:
/// it moves nothing in the code, but it can move a column in a diagnostic
/// or in debug information. With `-g3`, macro definitions are kept too
/// (`-dD`), since the object's debug information then has them.
///
/// The compiler itself maps `__FILE__` where the text uses it (it is given
/// the map's flags). It does not map the file names in its line markers,
/// so those are mapped here.
fn preprocess(program: &OsStr, compile: &Compile, map: Option<&PathMap>) -> Res<(String, u64)> {
    let mut command = Command::new(program);
    command.args(&compile.preprocess).args(["-E", "-C"]);
    if compile.macros_in_debug {
        command.arg("-dD");
    }
    if let Some(map) = map {
        command.args(map.flags());
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("Failed to run {} as a preprocessor", program.to_string_lossy()))?;
    let mut output = BufReader::new(child.stdout.take().context("the preprocessor has no output")?);

    let mut hasher = Hasher::new("text");
    let (mut line, mut bytes) = (Vec::new(), 0u64);
    loop {
        line.clear();
        if output.read_until(b'\n', &mut line).context("Failed to read the preprocessor's output")? == 0 {
            break;
        }
        // The length that closes the stream is the length of what went
        // into it: of the mapped text, which does not know how long the
        // installation's path is.
        match map {
            Some(map) if is_line_marker(&line) => {
                let mapped = map.apply(&line);
                hasher.stream(&mapped);
                bytes += mapped.len() as u64;
            }
            _ => {
                hasher.stream(&line);
                bytes += line.len() as u64;
            }
        }
    }
    hasher.end_stream(bytes);
    let status = child.wait().context("Failed to wait for the preprocessor")?;
    if !status.success() {
        bail!("the preprocessor failed ({status})");
    }
    Ok((hasher.hex(), bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objcache::Mode;
    use std::path::PathBuf;

    fn map(pairs: &[(&str, &'static str)]) -> PathMap {
        let mut from_to: Vec<(Vec<u8>, &'static str)> = pairs.iter().map(|(from, to)| (from.as_bytes().to_vec(), *to)).collect();
        from_to.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
        PathMap { from_to }
    }

    fn applied(map: &PathMap, text: &str) -> String {
        String::from_utf8(map.apply(text.as_bytes())).unwrap()
    }

    #[test]
    fn maps_whole_path_prefixes_most_specific_first() {
        let map = map(&[("/w/Cactus", ROOT_NAME), ("/w/Cactus/configs/sim", CONFIG_NAME)]);
        assert_eq!(applied(&map, "# 1 \"/w/Cactus/arrangements/A/T/src/a.c\""), "# 1 \"./arrangements/A/T/src/a.c\"");
        assert_eq!(applied(&map, "# 1 \"/w/Cactus/configs/sim/build/T/a.c\""), "# 1 \"./configs/@config/build/T/a.c\"");
        assert_eq!(applied(&map, "-isystem/w/Cactus/configs/sim"), "-isystem./configs/@config");
        assert_eq!(applied(&map, "# 1 \"/w/Cactus\""), "# 1 \".\"");
        // Another configuration, and a directory that merely starts alike.
        assert_eq!(applied(&map, "/w/Cactus/configs/simple/x"), "./configs/simple/x");
        assert_eq!(applied(&map, "/w/Cactus-2026/x /w/Cactusx"), "/w/Cactus-2026/x /w/Cactusx");
        // The flags name the less specific directory first.
        let flags: Vec<String> = map.flags().into_iter().map(|f| f.into_string().unwrap()).collect();
        assert_eq!(flags, ["-ffile-prefix-map=/w/Cactus=.", "-ffile-prefix-map=/w/Cactus/configs/sim=./configs/@config"]);
    }

    #[test]
    fn recognizes_line_markers() {
        assert!(is_line_marker(b"# 12 \"a.h\" 1\n"));
        assert!(is_line_marker(b"# 0 \"<built-in>\"\n"));
        for not in [&b"#define X 1\n"[..], b"#pragma once\n", b"# pragma omp\n", b"int x; # 1\n", b"#\n", b""] {
            assert!(!is_line_marker(not), "{}", String::from_utf8_lossy(not));
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
        };
        let key = parts.key();
        for change in [
            |p: &mut Parts| p.platform.push('x'),
            |p: &mut Parts| p.compiler.push('x'),
            |p: &mut Parts| p.arguments.push('x'),
            |p: &mut Parts| p.environment.push('x'),
            |p: &mut Parts| p.text.push('x'),
        ] {
            let mut changed = parts.clone();
            change(&mut changed);
            assert_ne!(changed.key(), key);
        }
    }

    /// A tree with one source and one header, in a Cactus-like layout.
    struct Tree {
        _tmp: tempfile::TempDir,
        conf: BuildConf,
        cc: PathBuf,
    }

    impl Tree {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(tmp.path()).unwrap().join("Cactus");
            let config = root.join("configs/sim");
            let cc = config.join(".cactup-builds/0000/cc");
            for dir in [&cc, &config.join("build/T"), &config.join("scratch"), &root.join("arrangements/A/T/src")] {
                std::fs::create_dir_all(dir).unwrap();
            }
            std::fs::write(root.join("arrangements/A/T/src/t.h"), "#define ANSWER 42 /* the answer */\n").unwrap();
            std::fs::write(config.join("build/T/a.c"), "#include \"t.h\"\nconst char *file = __FILE__;\nint answer = ANSWER;\n")
                .unwrap();
            let conf = BuildConf {
                mode: Mode::Record,
                cactup: PathBuf::from("/opt/cactup"),
                config_dir: config,
                cactus_root: root,
                machine: "test".into(),
                universe: None,
                build_env_digest: String::new(),
            };
            Self { _tmp: tmp, conf, cc }
        }

        fn argv(&self, flags: &[&str]) -> Vec<OsString> {
            let build = self.conf.config_dir.join("build/T");
            let mut argv = vec![OsString::from("gcc")];
            argv.extend(flags.iter().map(OsString::from));
            argv.extend([OsString::from("-c"), "-o".into(), build.join("a.c.o").into(), build.join("a.c").into()]);
            argv.push(format!("-I{}", self.conf.cactus_root.join("arrangements/A/T/src").display()).into());
            argv
        }

        fn key(&self, flags: &[&str]) -> Keyed {
            key(&self.conf, &self.cc, &self.argv(flags)).unwrap()
        }
    }

    fn have_gcc() -> bool {
        let found = identity::find_program(OsStr::new("gcc"));
        found.is_ok_and(|gcc| identity::identify(&std::env::temp_dir(), gcc.as_os_str()).is_ok_and(|c| c.family == Family::Gcc))
    }

    #[test]
    fn the_same_sources_in_another_installation_key_the_same() {
        if !have_gcc() {
            eprintln!("skipped: no GCC on this host");
            return;
        }
        let (here, there) = (Tree::new(), Tree::new());
        assert_ne!(here.conf.cactus_root, there.conf.cactus_root);
        let (a, b) = (here.key(&["-O2", "-g"]), there.key(&["-O2", "-g"]));
        assert_eq!(a.parts, b.parts, "nothing in the key may know where the tree is");
        assert_eq!(a.parts.key(), b.parts.key());
        assert!(a.still_holds());

        // And the same in a configuration of another name.
        let renamed = Tree::new();
        let other = renamed.conf.cactus_root.join("configs/sim-debug");
        std::fs::rename(&renamed.conf.config_dir, &other).unwrap();
        let renamed = Tree { conf: BuildConf { config_dir: other.clone(), ..renamed.conf }, cc: other.join(".cactup-builds/0000/cc"), _tmp: renamed._tmp };
        assert_eq!(renamed.key(&["-O2", "-g"]).parts, a.parts);
    }

    #[test]
    fn what_changes_the_object_changes_the_key() {
        if !have_gcc() {
            eprintln!("skipped: no GCC on this host");
            return;
        }
        let tree = Tree::new();
        let base = tree.key(&["-O2"]);
        let header = tree.conf.cactus_root.join("arrangements/A/T/src/t.h");
        let source = tree.conf.config_dir.join("build/T/a.c");

        // A flag that decides code generation: the arguments differ.
        let optimized = tree.key(&["-O3"]);
        assert_ne!(optimized.parts.arguments, base.parts.arguments);
        assert_eq!(optimized.parts.text, base.parts.text);
        // A macro on the command line: the text differs, the arguments do not.
        let defined = tree.key(&["-O2", "-DANSWER_OVERRIDE=1"]);
        assert_eq!(defined.parts.arguments, base.parts.arguments);
        assert_eq!(defined.parts.text, base.parts.text, "an unused macro changes nothing");
        let used = tree.key(&["-O2", "-Dfile=renamed"]);
        assert_ne!(used.parts.text, base.parts.text);

        // A header's code, and even a comment in it. (Not one inside a
        // directive: the preprocessor drops those with the directive, and
        // they move nothing in what the compiler sees.)
        std::fs::write(&header, "#define ANSWER 42 /* still the answer */\n").unwrap();
        assert_eq!(tree.key(&["-O2"]).parts.text, base.parts.text);
        std::fs::write(&header, "/* a comment of its own */\n#define ANSWER 42 /* the answer */\n").unwrap();
        let commented = tree.key(&["-O2"]);
        assert_ne!(commented.parts.text, base.parts.text);
        assert!(!base.still_holds(), "the text keyed before the edit is no longer the text");
        std::fs::write(&header, "/* a comment of its own */\n#define ANSWER 43 /* the answer */\n").unwrap();
        assert_ne!(tree.key(&["-O2"]).parts.text, commented.parts.text);

        // The source itself.
        std::fs::write(&source, "int answer = 1;\n").unwrap();
        assert_ne!(tree.key(&["-O2"]).parts.text, base.parts.text);
        // Put back, everything keys as it did: old entries stay useful.
        std::fs::write(&header, "#define ANSWER 42 /* the answer */\n").unwrap();
        std::fs::write(&source, "#include \"t.h\"\nconst char *file = __FILE__;\nint answer = ANSWER;\n").unwrap();
        assert_eq!(tree.key(&["-O2"]).parts, base.parts);

        // Another machine keys differently, with nothing else changed.
        let elsewhere = BuildConf { machine: "saturn".into(), ..tree.conf.clone() };
        let there = key(&elsewhere, &tree.cc, &tree.argv(&["-O2"])).unwrap();
        assert_ne!(there.parts.platform, base.parts.platform);
        assert_eq!(there.parts.text, base.parts.text);
    }

    #[test]
    fn what_it_cannot_key_says_why() {
        if !have_gcc() {
            eprintln!("skipped: no GCC on this host");
            return;
        }
        let tree = Tree::new();
        let why = |argv: Vec<OsString>| key(&tree.conf, &tree.cc, &argv).unwrap_err();
        assert!(why(tree.argv(&["-MD"])).contains("-MD is a flag"));
        assert!(why(vec!["gcc".into(), "-c".into(), "-o".into(), "x.o".into(), "missing.c".into()]).contains("the preprocessor failed"));
        let mut not_gcc = tree.argv(&[]);
        not_gcc[0] = "cat".into();
        assert!(why(not_gcc).contains("is not a compiler cactup knows"));
    }
}
