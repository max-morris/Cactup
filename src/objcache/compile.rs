//! What one compiler command line asks for, as far as the cache needs to
//! know: is it a compile the cache understands *completely*, and if so,
//! which arguments decide the object and which only feed the preprocessor.
//!
//! This reader is for the GCC and Clang drivers, gfortran's included. It
//! works from a list of
//! what it knows (§18.1 rule 1): a flag that is not on it makes the whole
//! command line "not cached", whatever the flag would have done. A miss
//! costs a compile; a flag misread could cost a wrong object.
//!
//! Two families are on the list as families, by prefix, because their
//! members are too many to name and none of them names a file: warning
//! options (`-W…`, which change diagnostics and the exit status only) and
//! machine options (`-m…`). Everything else is named one by one — the
//! debug and optimization levels too, since `-g…` and `-O…` also begin
//! flags that record the command line or the source in the object.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// The source language of a compile the cache understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    C,
    Cxx,
    /// Fortran that the driver does not preprocess (§18.10): Cactus hands
    /// gfortran the copies it has preprocessed itself. Fixed or free form,
    /// by the suffix.
    Fortran { fixed: bool },
}

impl Language {
    pub fn name(self) -> &'static str {
        match self {
            Self::C => "c",
            Self::Cxx => "c++",
            Self::Fortran { fixed: true } => "fortran-fixed",
            Self::Fortran { fixed: false } => "fortran-free",
        }
    }

    pub fn is_fortran(self) -> bool {
        matches!(self, Self::Fortran { .. })
    }

    /// By the source file's suffix, as the compiler driver decides it.
    fn of_source(source: &Path) -> Result<Self, String> {
        match source.extension().and_then(OsStr::to_str) {
            Some("c") => Ok(Self::C),
            Some("cc" | "cp" | "cxx" | "cpp" | "CPP" | "c++" | "C") => Ok(Self::Cxx),
            Some("f" | "for" | "ftn") => Ok(Self::Fortran { fixed: true }),
            Some("f90" | "f95" | "f03" | "f08") => Ok(Self::Fortran { fixed: false }),
            // gfortran runs the C preprocessor on these first, which this
            // reader does not follow for Fortran.
            Some(other @ ("F" | "FOR" | "FTN" | "FPP" | "fpp" | "F90" | "F95" | "F03" | "F08")) => {
                Err(format!("sources ending in .{other}, which the compiler preprocesses, are not cached"))
            }
            Some(other) => Err(format!("sources ending in .{other} are not cached")),
            None => Err("the source file has no suffix to tell its language by".to_owned()),
        }
    }
}

/// A compile of one source file to one object.
#[derive(Debug, PartialEq)]
pub struct Compile {
    pub language: Language,
    pub source: PathBuf,
    /// Where the source stands among the arguments: a Fortran compile under
    /// the path map compiles a renamed copy in its place (§18.10).
    pub source_at: usize,
    /// Where `-c`, and `-o` with its file, stand among the arguments.
    output_at: Vec<usize>,
    pub output: PathBuf,
    /// The command line of the matching preprocessor run, minus the `-E`:
    /// every argument but `-c` and `-o <file>`.
    pub preprocess: Vec<OsString>,
    /// The arguments that go into the key as they are: every argument but
    /// `-c`, `-o <file>`, the source file, and `-I`/`-D`/`-U`, whose whole
    /// effect is in the preprocessed text. In the order given: for most
    /// flags the last one wins, and cactup does not know for which.
    pub keyed: Vec<OsString>,
    /// Debug information is on (`-g…`, not `-g0`): the object then also
    /// records where it was compiled.
    pub debug: bool,
    /// `-g3`: the object's debug information also records macros, which the
    /// preprocessed text alone does not show.
    pub macros_in_debug: bool,
    /// `-fopenmp`: Clang then puts source locations in the object that
    /// `-ffile-prefix-map` does not reach.
    pub openmp: bool,
    /// `-include` is given: Clang looks for a precompiled header beside
    /// such a file.
    pub forced_include: bool,
    /// `-march=native` or one of its like: the compiler targets the
    /// processor it finds itself running on.
    pub native: bool,
    /// `-finput-charset=` or `-fexec-charset=`: character set conversion,
    /// which can follow the locale (`ASCII//TRANSLIT` does), so the locale
    /// stays in the key whatever the compiler's locale trial said.
    pub charset: bool,
    /// The flags that have the compiler write a dependency file while it
    /// compiles (`-MD -MP -MF <file> -MT <target>`), as given. They change
    /// neither the object nor the preprocessed text, so they are not in the
    /// key; and they are kept from the preprocessor runs that make the key,
    /// which would write the file too. (A cache that serves an object has
    /// to see that file written: that is what they are kept for.)
    pub depend: Vec<OsString>,
}

/// Flags that take their value as the next argument, and go into the key.
const WITH_VALUE: &[&str] = &[
    "-isystem",
    "-iquote",
    "-idirafter",
    "-include",
    "-iprefix",
    "-iwithprefix",
    "-iwithprefixbefore",
    "-isysroot",
    "--param",
];

/// Flags the cache knows it cannot follow: they name inputs the
/// preprocessed text does not show, or outputs besides the object, or
/// another program that takes part in the compile. Matched as prefixes.
const NOT_CACHED: &[&str] = &[
    // Not compiles to an object (`-M`, `-MM`; with the rest of the `-M…`
    // flags that `parse` does not take as dependency output).
    "-E", "-S", "-M",
    // Its effect depends on where it stands among the input files.
    "-x",
    // Reads a file the preprocessor's output does not name.
    "-imacros",
    // Read lists from the compiler's own directories, and put source paths
    // where no path map reaches.
    "-fsanitize", "-fno-sanitize",
    // Hands the next argument to LLVM; Clang's module options (`-m…` by
    // spelling only).
    "-mllvm", "-module",
    // More inputs: plugins, specs, profiles, lists, precompiled headers,
    // modules, link-time optimization.
    "@", "-fplugin", "-specs", "--specs", "-Xclang", "-Xpreprocessor", "-Xassembler", "-Wp,", "-Wa,", "-B",
    "-wrapper", "-fprofile-", "-fauto-profile", "-fbranch-probabilities", "-fxray-", "-fthinlto-index",
    "-flto", "-fno-lto", "-fmodule", "-fpch", "-include-pch", "-fopenmp-targets", "-foffload", "-ipo",
    // Fortran (§18.10): a module directory of its own, more module search
    // directories or a file read first, the preprocessor, no object at all.
    "-J", "-fintrinsic-modules-path", "-fpre-include", "-cpp", "-fsyntax-only", "-fc-prototypes", "-fopenacc",
    // More outputs.
    "--coverage", "-ftest-coverage", "-gsplit-dwarf", "-fstack-usage", "-ftime-trace", "-ftime-report",
    "-fdump-", "-save-temps", "-frecord-gcc-switches", "-fcallgraph-info", "-fopt-info", "-aux-info",
    "-dumpbase", "-dumpdir", "-fdiagnostics-format",
];

/// `-f<name>` and `-fno-<name>` flags that only change the object or the
/// diagnostics, with no file but the object written and none read.
const F_FLAGS: &[&str] = &[
    "PIC", "pic", "PIE", "pie", "openmp", "openmp-simd", "strict-aliasing", "fast-math", "math-errno",
    "unsafe-math-optimizations", "finite-math-only", "associative-math", "reciprocal-math", "signed-zeros",
    "trapping-math", "rounding-math", "signaling-nans", "unroll-loops", "unroll-all-loops",
    "omit-frame-pointer", "exceptions", "rtti", "stack-protector", "stack-protector-strong",
    "stack-protector-all", "stack-clash-protection", "common", "tree-vectorize", "tree-loop-vectorize",
    "tree-slp-vectorize", "vectorize", "slp-vectorize", "signed-char", "unsigned-char", "wrapv", "trapv",
    "inline", "inline-functions", "inline-small-functions", "permissive", "check-new", "function-sections",
    "data-sections", "asynchronous-unwind-tables", "unwind-tables", "plt", "semantic-interposition",
    "delete-null-pointer-checks", "strict-overflow", "builtin", "gnu89-inline", "lax-vector-conversions",
    "ms-extensions", "threadsafe-statics", "keep-inline-functions", "merge-all-constants", "short-enums",
    "ident", "expensive-optimizations", "peel-loops", "prefetch-loop-arrays", "tree-loop-distribution",
    "loop-interchange", "cx-limited-range", "cx-fortran-rules", "elide-constructors", "implicit-templates",
    "sized-deallocation", "aligned-new", "char8_t", "concepts", "coroutines", "show-column",
    "diagnostics-show-option", "diagnostics-show-caret", "caret-diagnostics", "color-diagnostics",
    "colored-diagnostics", "spell-checking", "float-store", "pack-struct",
];

/// `-f<name>=<value>` flags of the same kind.
const F_FLAGS_WITH_VALUE: &[&str] = &[
    "visibility", "diagnostics-color", "message-length", "fp-contract", "excess-precision", "template-depth",
    "constexpr-depth", "constexpr-loop-limit", "constexpr-steps", "max-errors", "error-limit", "inline-limit", "tls-model", "cf-protection", "debug-prefix-map",
    "file-prefix-map", "macro-prefix-map", "abi-version", "align-functions", "align-loops", "align-jumps",
    "align-labels", "tabstop", "input-charset", "exec-charset", "openmp-version", "vect-cost-model",
    "simd-cost-model", "pack-struct", "random-seed", "zero-call-used-regs", "strict-flex-arrays", "fp-model",
];

/// gfortran's `-f<name>` and `-fno-<name>` flags that only change the object
/// or the diagnostics (§18.10): the source form, the meaning of types and of
/// old extensions, runtime checks, how arrays and calls are made.
const FORTRAN_F_FLAGS: &[&str] = &[
    "fixed-form", "free-form", "fixed-line-length-none", "free-line-length-none", "cray-pointer", "dollar-ok",
    "backslash", "implicit-none", "default-real-8", "default-real-10", "default-real-16", "default-double-8",
    "default-integer-8", "integer-4-integer-8", "real-4-real-8", "real-4-real-10", "real-4-real-16",
    "real-8-real-4", "real-8-real-10", "real-8-real-16", "range-check", "d-lines-as-code", "d-lines-as-comments",
    "allow-argument-mismatch", "allow-invalid-boz", "allow-leading-underscore", "bounds-check",
    "check-array-temporaries", "backtrace", "dump-core", "init-local-zero", "init-derived", "external-blas",
    "automatic", "recursive", "stack-arrays", "realloc-lhs", "protect-parens", "aggressive-function-elimination",
    "frontend-optimize", "sign-zero", "underscoring", "second-underscore", "align-commons", "inline-arg-packing",
    "pad-source", "repack-arrays", "short-enums", "whole-file", "dec", "dec-structure", "dec-intrinsic-ints",
    "dec-static", "dec-math", "dec-include", "dec-format-defaults", "dec-blank-format-item", "dec-char-conversions",
];

/// gfortran's `-f<name>=<value>` flags of the same kind.
const FORTRAN_F_FLAGS_WITH_VALUE: &[&str] = &[
    "init-real", "init-integer", "init-logical", "init-character", "check", "convert", "record-marker",
    "max-subrecord-length", "max-stack-var-size", "max-array-constructor", "blas-matmul-limit",
    "inline-matmul-limit", "coarray", "fpe-trap", "fpe-summary", "max-identifier-length",
];

/// Is `flag` one of gfortran's own plain settings?
fn is_fortran_setting(flag: &str) -> bool {
    if flag == "-nocpp" {
        return true;
    }
    let Some(name) = flag.strip_prefix("-f") else { return false };
    let name = name.strip_prefix("no-").unwrap_or(name);
    // `-ffixed-line-length-132`, `-ffree-line-length-0`.
    if let Some(columns) = ["fixed-line-length-", "free-line-length-"].iter().find_map(|prefix| name.strip_prefix(prefix)) {
        return columns == "none" || (!columns.is_empty() && columns.bytes().all(|b| b.is_ascii_digit()));
    }
    match name.split_once('=') {
        Some((name, _)) => FORTRAN_F_FLAGS_WITH_VALUE.contains(&name),
        None => FORTRAN_F_FLAGS.contains(&name),
    }
}

/// The debug level a `-g…` flag asks for, if it is one of the plain level
/// flags. `-g` and `-ggdb` ask for the default level, 2. Other `-g…` flags
/// exist that record the command line or embed the source in the object
/// (`-grecord-command-line`, `-gembed-source`): those are not known here.
fn debug_level(flag: &str) -> Option<u8> {
    match flag {
        "-g0" | "-ggdb0" => Some(0),
        "-g1" | "-ggdb1" => Some(1),
        "-g" | "-g2" | "-ggdb" | "-ggdb2" | "-gdwarf" | "-gdwarf-2" | "-gdwarf-3" | "-gdwarf-4" | "-gdwarf-5" => Some(2),
        "-g3" | "-ggdb3" => Some(3),
        _ => None,
    }
}

/// Is `flag` one of the flags the cache knows to be a plain setting?
fn is_setting(flag: &str) -> bool {
    if matches!(flag, "-w" | "-ansi" | "-pedantic" | "-pedantic-errors" | "-pthread" | "-pipe" | "-nostdinc"
        | "-nostdinc++" | "-undef" | "-trigraphs" | "-rdynamic" | "-shared" | "-static"
        | "-O" | "-O0" | "-O1" | "-O2" | "-O3" | "-Os" | "-Og" | "-Oz" | "-Ofast")
        || debug_level(flag).is_some()
    {
        return true;
    }
    if let Some(name) = flag.strip_prefix("-f") {
        let name = name.strip_prefix("no-").unwrap_or(name);
        return match name.split_once('=') {
            Some((name, _)) => F_FLAGS_WITH_VALUE.contains(&name),
            None => F_FLAGS.contains(&name),
        };
    }
    // `-std=gnu99`; the warning and the machine options, as families.
    ["-std=", "-W", "-m"].iter().any(|prefix| flag.starts_with(prefix))
}

/// Read the arguments of one compiler command line (everything after the
/// program's name). `Err` says why this command line is not cached; it
/// still compiles exactly as given.
pub fn parse(args: &[OsString]) -> Result<Compile, String> {
    let total = args.len();
    let mut args = args.iter();
    let mut compile_only = false;
    let (mut source, mut output, mut source_at) = (None, None, 0);
    let mut output_at = Vec::new();
    let (mut preprocess, mut keyed, mut depend) = (Vec::new(), Vec::new(), Vec::new());
    let (mut level, mut openmp, mut forced_include, mut native, mut charset) = (0, false, false, false, false);
    let mut depend_file = false;
    // Flags the general list does not know: gfortran's own, if the source
    // turns out to be Fortran (its suffix may come last).
    let mut unknown = Vec::new();

    while let Some(arg) = args.next() {
        let at = total - args.len() - 1;
        let Some(flag) = arg.to_str() else {
            return Err("an argument is not valid UTF-8".to_owned());
        };
        // `-o file`, `-ofile`, `-I dir`, `-Idir`, …: the value, wherever it is.
        let mut value_of = |name: &str| -> Result<Option<OsString>, String> {
            match flag.strip_prefix(name) {
                Some("") => match args.next() {
                    Some(value) => Ok(Some(value.clone())),
                    None => Err(format!("{name} has nothing after it")),
                },
                Some(joined) => Ok(Some(OsString::from(joined))),
                None => Ok(None),
            }
        };

        if flag == "-c" {
            compile_only = true;
            output_at.push(at);
        } else if let Some(file) = value_of("-o")? {
            if output.replace(PathBuf::from(file)).is_some() {
                return Err("-o is given more than once".to_owned());
            }
            output_at.push(at);
            if flag == "-o" {
                output_at.push(at + 1);
            }
        } else if matches!(flag, "-MD" | "-MMD" | "-MP") {
            // A dependency file written while compiling.
            depend.push(arg.clone());
        } else if let Some((name, value)) = ["-MF", "-MT", "-MQ"].iter().find_map(|name| Some((name, value_of(name).transpose()?))) {
            let value = value?;
            // Dependencies on standard output would be in the preprocessed
            // text the key reads.
            if *name == "-MF" && value == "-" {
                return Err("the dependency file is standard output".to_owned());
            }
            depend_file |= *name == "-MF";
            depend.push(OsString::from(name));
            depend.push(value);
        } else if NOT_CACHED.iter().any(|prefix| flag.starts_with(prefix)) {
            return Err(format!("{flag} is a flag the cache does not follow"));
        } else if let Some(value) = ["-I", "-D", "-U"].iter().find_map(|name| value_of(name).transpose()) {
            // Their whole effect is in the preprocessed text.
            preprocess.push(OsString::from(&flag[..2]));
            preprocess.push(value?);
        } else if WITH_VALUE.contains(&flag) {
            let Some(value) = args.next() else {
                return Err(format!("{flag} has nothing after it"));
            };
            forced_include |= flag == "-include";
            for list in [&mut preprocess, &mut keyed] {
                list.push(arg.clone());
                list.push(value.clone());
            }
        } else if flag == "-" {
            return Err("the source comes from standard input".to_owned());
        } else if flag.starts_with('-') {
            if !is_setting(flag) {
                unknown.push(flag.to_owned());
            }
            // An explicit level sets the level; a bare `-g` only turns
            // debug information on, and leaves a level already asked for.
            match debug_level(flag) {
                Some(2) if matches!(flag, "-g" | "-ggdb") || flag.starts_with("-gdwarf") => level = level.max(2),
                Some(asked) => level = asked,
                None => {}
            }
            openmp |= flag == "-fopenmp";
            // Not undone by a later `-march=<name>`: the tuning may still
            // be the host's, and which flag wins is the compiler's to say.
            // (`-mcpu=native+nosve`: with extensions it is still the host.)
            native |= ["-march=native", "-mtune=native", "-mcpu=native"].iter().any(|native| flag.starts_with(native));
            charset |= ["-finput-charset=", "-fexec-charset="].iter().any(|charset| flag.starts_with(charset));
            preprocess.push(arg.clone());
            keyed.push(arg.clone());
        } else {
            if source.replace(PathBuf::from(arg)).is_some() {
                return Err("there is more than one input file".to_owned());
            }
            source_at = at;
            preprocess.push(arg.clone());
        }
    }

    if !compile_only {
        return Err("it is not a compile to an object (no -c)".to_owned());
    }
    let source = source.ok_or("there is no input file")?;
    let output = output.ok_or("there is no -o")?;
    let language = Language::of_source(&source)?;
    if let Some(flag) = unknown.iter().find(|flag| !(language.is_fortran() && is_fortran_setting(flag))) {
        return Err(format!("{flag} is not a flag the cache knows"));
    }
    // Cactus has the dependencies of Fortran from a run of its own; a
    // dependency file from the compile would list modules (§18.10).
    if language.is_fortran() && !depend.is_empty() {
        return Err("a dependency file is asked of a Fortran compile".to_owned());
    }
    // A path with a newline in it could pass for two lines of a
    // preprocessor's output.
    if source.as_os_str().as_bytes().contains(&b'\n') {
        return Err("the source file's name has a line break in it".to_owned());
    }
    let (debug, macros_in_debug) = (level > 0, level == 3);
    // Where the file goes without `-MF` depends on `-o`, which the
    // preprocessor runs do not have: a cache could not have it written.
    if !depend.is_empty() && !depend_file {
        return Err("a dependency file is asked for without -MF to say where".to_owned());
    }
    Ok(Compile { language, source, source_at, output_at, output, preprocess, keyed, debug, macros_in_debug, openmp, forced_include, native, charset, depend })
}

impl Compile {
    /// `args`, the arguments this was read from, without `-c` and `-o` with
    /// its file: a run that writes no object. With where the source then
    /// stands.
    pub fn without_output(&self, args: &[OsString]) -> (Vec<OsString>, usize) {
        let kept: Vec<OsString> = args.iter().enumerate().filter(|(at, _)| !self.output_at.contains(at)).map(|(_, arg)| arg.clone()).collect();
        let source_at = self.source_at - self.output_at.iter().filter(|at| **at < self.source_at).count();
        (kept, source_at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn parsed(args: &[&str]) -> Result<Compile, String> {
        parse(&os(args))
    }

    /// The shape of a Cactus compile line.
    const CACTUS: &[&str] = &[
        "-g", "-std=gnu99", "-O3", "-c", "-o", "/c/build/T/a.c.o", "/c/build/T/a.c", "-I/src/T", "-I", "/c/bindings",
        "-DCCODE", "-DX=1",
    ];

    #[test]
    fn reads_a_cactus_compile() {
        let compile = parsed(CACTUS).unwrap();
        assert_eq!(compile.language, Language::C);
        assert_eq!(compile.source, Path::new("/c/build/T/a.c"));
        assert_eq!(CACTUS[compile.source_at], "/c/build/T/a.c");
        let (without, at) = compile.without_output(&os(CACTUS));
        assert_eq!(without, os(&["-g", "-std=gnu99", "-O3", "/c/build/T/a.c", "-I/src/T", "-I", "/c/bindings", "-DCCODE", "-DX=1"]));
        assert_eq!(without[at], "/c/build/T/a.c");
        let joined = parsed(&["-ox.o", "a.f90", "-c", "-g"]).unwrap();
        let (without, at) = joined.without_output(&os(&["-ox.o", "a.f90", "-c", "-g"]));
        assert_eq!((without, at), (os(&["a.f90", "-g"]), 0));
        assert_eq!(compile.output, Path::new("/c/build/T/a.c.o"));
        assert_eq!(compile.keyed, os(&["-g", "-std=gnu99", "-O3"]));
        assert_eq!(
            compile.preprocess,
            os(&["-g", "-std=gnu99", "-O3", "/c/build/T/a.c", "-I", "/src/T", "-I", "/c/bindings", "-D", "CCODE", "-D", "X=1"])
        );
        assert!(compile.debug && !compile.macros_in_debug);
    }

    #[test]
    fn tells_the_language_by_suffix_or_by_flag() {
        for (source, language) in [("a.c", Language::C), ("a.cc", Language::Cxx), ("a.cxx", Language::Cxx), ("a.C", Language::Cxx)] {
            assert_eq!(parsed(&["-c", "-o", "a.o", source]).unwrap().language, language, "{source}");
        }
        for (source, fixed) in [("a.f", true), ("a.for", true), ("a.ftn", true), ("a.f90", false), ("a.f08", false)] {
            assert_eq!(parsed(&["-c", "-o", "a.o", source]).unwrap().language, Language::Fortran { fixed }, "{source}");
        }
        for source in ["a.F", "a.F90", "a.fpp", "a.cu", "a.S", "a.m", "a"] {
            assert!(parsed(&["-c", "-o", "a.o", source]).is_err(), "{source}");
        }
        assert!(parsed(&["-c", "-o", "a.o", "a.F90"]).unwrap_err().contains("preprocesses"));
        // `-x` means one thing before the source and another after it.
        for args in [&["-x", "c++", "-c", "-o", "a.o", "a.c"][..], &["-c", "-o", "a.o", "a.c", "-x", "c++"], &["-xc", "-c", "-o", "a.o", "a.cc"]] {
            assert!(parsed(args).unwrap_err().contains("does not follow"), "{args:?}");
        }
    }

    #[test]
    fn debug_information_is_noticed() {
        let debug = |flags: &[&str]| {
            let mut args = flags.to_vec();
            args.extend(["-c", "-o", "a.o", "a.c"]);
            let compile = parsed(&args).unwrap();
            (compile.debug, compile.macros_in_debug)
        };
        assert_eq!(debug(&[]), (false, false));
        assert_eq!(debug(&["-g"]), (true, false));
        assert_eq!(debug(&["-gdwarf-5"]), (true, false));
        assert_eq!(debug(&["-ggdb3"]), (true, true));
        // A bare `-g` after `-g3` leaves the level at 3, as GCC does.
        assert_eq!(debug(&["-g3", "-g"]), (true, true));
        assert_eq!(debug(&["-g3", "-g2"]), (true, false));
        assert_eq!(debug(&["-g", "-g0"]), (false, false));
        assert_eq!(debug(&["-g0", "-g"]), (true, false));
    }

    #[test]
    fn notices_what_the_key_needs_to_know_about() {
        let base = parsed(&["-c", "-o", "a.o", "a.c"]).unwrap();
        assert!(!base.openmp && !base.forced_include && !base.native);
        for flag in ["-march=native", "-mtune=native", "-mcpu=native"] {
            assert!(parsed(&[flag, "-march=x86-64-v3", "-c", "-o", "a.o", "a.c"]).unwrap().native, "{flag}");
        }
        assert!(!parsed(&["-march=x86-64-v3", "-c", "-o", "a.o", "a.c"]).unwrap().native);
        assert!(parsed(&["-fopenmp", "-c", "-o", "a.o", "a.c"]).unwrap().openmp);
        assert!(!parsed(&["-fno-openmp", "-c", "-o", "a.o", "a.c"]).unwrap().openmp);
        assert!(parsed(&["-include", "pre.h", "-c", "-o", "a.o", "a.c"]).unwrap().forced_include);
        assert!(parsed(&["-imacros", "pre.h", "-c", "-o", "a.o", "a.c"]).unwrap_err().contains("does not follow"));
        assert!(parsed(&["-mcpu=native+nosve", "-c", "-o", "a.o", "a.c"]).unwrap().native);
    }

    #[test]
    fn a_dependency_file_written_while_compiling_is_set_aside() {
        // What the Cactus recipe gives when dependencies come from the
        // compile: neither keyed nor handed to the preprocessor.
        let base = parsed(CACTUS).unwrap();
        let mut args = vec!["-MD", "-MP", "-MF", "/c/build/T/a.c.d", "-MT", "a.c.o"];
        args.extend(CACTUS);
        let compile = parsed(&args).unwrap();
        assert_eq!(compile.depend, os(&["-MD", "-MP", "-MF", "/c/build/T/a.c.d", "-MT", "a.c.o"]));
        assert_eq!((&compile.keyed, &compile.preprocess), (&base.keyed, &base.preprocess));
        assert!(base.depend.is_empty());
        // Joined values, and the other spellings.
        let compile = parsed(&["-MMD", "-MFa.d", "-MQ", "a b.o", "-c", "-o", "a.o", "a.c"]).unwrap();
        assert_eq!(compile.depend, os(&["-MMD", "-MF", "a.d", "-MQ", "a b.o"]));
    }

    #[test]
    fn flags_with_values_keep_them() {
        let compile = parsed(&["-isystem", "/opt/inc", "--param", "x=1", "-include", "pre.h", "-c", "-o", "a.o", "a.c"]).unwrap();
        assert_eq!(compile.keyed, os(&["-isystem", "/opt/inc", "--param", "x=1", "-include", "pre.h"]));
        assert!(parsed(&["-c", "-o", "a.o", "a.c", "-isystem"]).unwrap_err().contains("nothing after it"));
        assert!(parsed(&["-c", "a.c", "-o"]).unwrap_err().contains("nothing after it"));
    }

    #[test]
    fn what_it_does_not_fully_understand_is_not_cached() {
        for (args, why) in [
            (&["-o", "a.o", "a.c"][..], "no -c"),
            (&["-c", "a.c"], "no -o"),
            (&["-c", "-o", "a.o"], "no input file"),
            (&["-c", "-o", "a.o", "a.c", "b.c"], "more than one input"),
            (&["-c", "-o", "a.o", "-o", "b.o", "a.c"], "more than once"),
            (&["-E", "-o", "a.i", "a.c"], "-E is a flag"),
            (&["-c", "-o", "a.o", "a.c", "-MD"], "without -MF"),
            // `-MF` as the *value* of another flag is a target's name.
            (&["-c", "-o", "a.o", "a.c", "-MD", "-MT", "-MF"], "without -MF"),
            (&["-c", "-o", "a.o", "a.c", "-MD", "-MQ", "-MF"], "without -MF"),
            (&["-c", "-o", "a.o", "a.c", "-M"], "-M is a flag"),
            (&["-c", "-o", "a.o", "a.c", "-MM"], "-MM is a flag"),
            (&["-c", "-o", "a.o", "a.c", "-MG", "-MD", "-MF", "a.d"], "-MG is a flag"),
            (&["-c", "-o", "a.o", "a.c", "-MD", "-MF"], "nothing after it"),
            (&["-c", "-o", "a.o", "a.c", "-fprofile-use=p"], "does not follow"),
            (&["-c", "-o", "a.o", "a.c", "-flto"], "does not follow"),
            (&["-c", "-o", "a.o", "a.c", "@args"], "does not follow"),
            (&["-c", "-o", "a.o", "a.c", "-Wa,-adhln"], "does not follow"),
            (&["-c", "-o", "a.o", "a.c", "-fsanitize-ignorelist=l"], "does not follow"),
            (&["-c", "-o", "a.o", "a.c", "-fsanitize=address"], "does not follow"),
            (&["-c", "-o", "a.o", "a.c", "-grecord-command-line"], "not a flag the cache knows"),
            (&["-c", "-o", "a.o", "a.c", "-gembed-source"], "not a flag the cache knows"),
            (&["-c", "-o", "a.o", "a.c", "-gsplit-dwarf"], "does not follow"),
            (&["-c", "-o", "a.o", "a.c", "-O9"], "not a flag the cache knows"),
            (&["-c", "-o", "a.o", "a.c", "-mllvm", "-inline-threshold=1"], "does not follow"),
            (&["-c", "-o", "a.o", "a.c", "-fnew-flag-of-next-year"], "not a flag the cache knows"),
            (&["-c", "-o", "a.o", "a.c", "--weird"], "not a flag the cache knows"),
            (&["-c", "-o", "a.o", "-"], "standard input"),
        ] {
            let err = parsed(args).unwrap_err();
            assert!(err.contains(why), "{args:?}: {err}");
        }
    }

    /// The shape of a Cactus Fortran compile line (`COMPILE_F90`), with the
    /// settings the machine database's optionlists give gfortran.
    #[test]
    fn reads_a_cactus_fortran_compile() {
        let compile = parsed(&[
            "-g", "-fcray-pointer", "-ffixed-line-length-none", "-O3", "-funroll-loops", "-fopenmp", "-Wall",
            "-finit-real=nan", "-fcheck=bounds,mem", "-fno-range-check", "-ffree-line-length-132", "-I/c/bindings",
            "-I", "/src/T", "-c", "-o", "/c/build/T/a.F90.o", "/c/build/T/a.f90",
        ])
        .unwrap();
        assert_eq!(compile.language, Language::Fortran { fixed: false });
        assert_eq!(compile.keyed.len(), 11);
        assert!(compile.openmp && compile.debug);
        // gfortran's own settings are only settings of a Fortran compile.
        assert!(parsed(&["-fcray-pointer", "-c", "-o", "a.o", "a.c"]).unwrap_err().contains("not a flag the cache knows"));
        for (args, why) in [
            (&["-J", "/m", "-c", "-o", "a.o", "a.f90"][..], "does not follow"),
            (&["-J/m", "-c", "-o", "a.o", "a.f90"], "does not follow"),
            (&["-M/m", "-c", "-o", "a.o", "a.f90"], "does not follow"),
            (&["-cpp", "-c", "-o", "a.o", "a.f90"], "does not follow"),
            (&["-fintrinsic-modules-path", "/x", "-c", "-o", "a.o", "a.f90"], "does not follow"),
            (&["-fpre-include=/x.h", "-c", "-o", "a.o", "a.f90"], "does not follow"),
            (&["-fsyntax-only", "-c", "-o", "a.o", "a.f90"], "does not follow"),
            (&["-ffixed-line-length-wide", "-c", "-o", "a.o", "a.f"], "not a flag the cache knows"),
            (&["-fnew-fortran-flag", "-c", "-o", "a.o", "a.f"], "not a flag the cache knows"),
            (&["-MD", "-MF", "a.d", "-c", "-o", "a.o", "a.f90"], "dependency file"),
        ] {
            let err = parsed(args).unwrap_err();
            assert!(err.contains(why), "{args:?}: {err}");
        }
        for flag in ["-ffixed-form", "-fno-underscoring", "-fdefault-real-8", "-fconvert=big-endian", "-fcoarray=single", "-nocpp", "-std=f2008"] {
            assert!(parsed(&[flag, "-c", "-o", "a.o", "a.f"]).is_ok(), "{flag}");
        }
    }

    #[test]
    fn settings_it_knows() {
        for flag in [
            "-O2", "-Ofast", "-g", "-ggdb", "-gdwarf-5", "-std=c++17", "-Wall", "-Wno-unused", "-Werror=vla", "-w",
            "-march=native", "-mavx2", "-mtune=generic", "-fPIC", "-fopenmp", "-fno-strict-aliasing", "-ffast-math",
            "-fvisibility=hidden", "-fno-omit-frame-pointer", "-fdiagnostics-color=always",
            "-ffile-prefix-map=/a=/b", "-pthread", "-pipe", "-rdynamic", "-D_GNU_SOURCE", "-UNDEBUG",
        ] {
            assert!(parsed(&[flag, "-c", "-o", "a.o", "a.c"]).is_ok(), "{flag}");
        }
    }
}
