//! The probe (§18.3): run by the build script after `make <config>-config`,
//! in the very context the compiles will run in (compute node, container
//! universe), to decide whether the wrapper can be used there and to write
//! the makefile fragment that puts it in front of the compilers.
//!
//! The fragment sets each compiler variable *for Cactus's object targets
//! only*, as a pattern-specific `private` variable:
//!
//! ```make
//! %.c.o: private CC = $(if $(cactup_cc_on),'<cactup>' __cc '<conf>' CC:1 )gcc
//! ```
//!
//! Three properties of that shape carry the design, and the build script's
//! self-test (see [`selftest_guarded`]) checks them against the `make` at
//! hand before the fragment is used:
//!
//! - it applies to targets named like Cactus's objects and to nothing else,
//!   so dependency generation (`$(CC) -E -M`), `datestamp.c`, configure runs
//!   and linking never see the wrapper;
//! - `private` keeps it from a target's prerequisites — an ExternalLibraries
//!   `build.sh` hangs off the objects that need it, and must get the real
//!   compiler;
//! - the guard limits it to sub-makes reading Cactus's `make.subdir`, so a
//!   third-party build that happens to name an object `foo.c.o` is left
//!   alone.
//!
//! A command-line override (`make CC='cactup __cc gcc'`) has none of them:
//! make exports it into every recipe's environment.

use super::{conf_path, inject_path, selftest_dir, BuildConf, Mode, PROBE_DECLINED, WRAP_VERB};
use crate::Res;
use anyhow::{bail, Context};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

/// Cactus's object rules (`lib/make/make.config.rules.in`): each compiler
/// variable with the object patterns whose recipe runs it. The probe injects
/// a pattern only where the configuration's own rules file still defines it,
/// so a flesh that renames its objects loses the cache, not its build.
const OBJECT_RULES: &[(&str, &[&str])] = &[
    ("CC", &["%.c.o"]),
    ("CXX", &["%.cc.o", "%.C.o", "%.cpp.o", "%.cxx.o"]),
    ("CUCC", &["%.cu.o"]),
    ("F77", &["%.F77.o", "%.f77.o"]),
    ("F90", &["%.F.o", "%.f.o", "%.F90.o", "%.f90.o"]),
];

/// The sub-makefile every Cactus object compile runs under; the fragment's
/// guard looks for this suffix in `MAKEFILE_LIST`.
const SUBDIR_MAKEFILE: &str = "lib/make/make.subdir";

/// Tools that already stand in front of a compiler. cactup does not stack
/// on top of one: which of the two would see the real compiler, and what
/// the other would then key on, is not something to guess at.
const OTHER_WRAPPERS: &[&str] =
    &["ccache", "sccache", "distcc", "icecc", "icerun", "buildcache", "f90cache", "cachecc1"];

/// The self-test makefile that stands in for a Cactus sub-make: it is named
/// like `make.subdir`, so the fragment's guard is on.
pub fn selftest_guarded(cc_dir: &Path) -> PathBuf {
    selftest_dir(cc_dir).join(SUBDIR_MAKEFILE)
}

/// The self-test makefile that stands in for any other build.
pub fn selftest_unguarded(cc_dir: &Path) -> PathBuf {
    selftest_dir(cc_dir).join("other.mk")
}

/// `cactup __cc-probe <config.toml>`: exit 0 with the fragment and the
/// self-test written, or [`PROBE_DECLINED`] with one line on stderr saying
/// why the cache stays out of this build.
pub fn run(conf: Option<OsString>) -> i32 {
    let outcome = match conf {
        Some(conf) => probe(Path::new(&conf)),
        None => Err(anyhow::anyhow!("the probe was run without a configuration file")),
    };
    match outcome {
        Ok(notes) => {
            for note in notes {
                eprintln!("cactup: build cache: {note}");
            }
            0
        }
        Err(e) => {
            eprintln!("cactup: build cache off for this build: {e:#}");
            PROBE_DECLINED
        }
    }
}

/// Returns the notes to print: compiler variables left unwrapped, and why.
fn probe(conf_file: &Path) -> Res<Vec<String>> {
    let conf = BuildConf::load(conf_file)?;
    let cc_dir = conf_file.parent().context("the configuration file has no parent directory")?;
    if conf_path(cc_dir) != conf_file {
        bail!("{} is not a build attempt's cache configuration", conf_file.display());
    }
    if conf.mode == Mode::Off {
        bail!("it is turned off");
    }
    if !conf.cactup.is_file() {
        bail!("{} is not visible here", conf.cactup.display());
    }
    // `MAKEFILES` is a whitespace-separated list: a fragment path with a
    // blank in it cannot be named there.
    let inject = inject_path(cc_dir);
    if inject.to_string_lossy().contains(char::is_whitespace) {
        bail!("the path {} contains whitespace, which make cannot take in MAKEFILES", inject.display());
    }
    if !conf.cactus_root.join(SUBDIR_MAKEFILE).is_file() {
        bail!(
            "{} is missing: this Cactus does not compile through the make.subdir sub-make cactup knows",
            conf.cactus_root.join(SUBDIR_MAKEFILE).display()
        );
    }

    let config_data = conf.config_dir.join("config-data");
    let defn_path = config_data.join("make.config.defn");
    let defn = fs::read_to_string(&defn_path).with_context(|| format!("cannot read {}", defn_path.display()))?;
    let rules_path = config_data.join("make.config.rules");
    let rules = fs::read_to_string(&rules_path).with_context(|| format!("cannot read {}", rules_path.display()))?;

    let mut notes = Vec::new();
    let mut wrapped = Vec::new();
    for (var, patterns) in OBJECT_RULES {
        let patterns: Vec<&str> = patterns.iter().copied().filter(|p| defines_rule(&rules, p)).collect();
        if patterns.is_empty() {
            continue;
        }
        match compiler_command(&defn, var) {
            Ok(words) => wrapped.push(Wrapped { var, patterns, words }),
            Err(Unwrapped::Unset) => {}
            Err(Unwrapped::Because(why)) => notes.push(format!("leaving {var} unwrapped ({why})")),
        }
    }
    if wrapped.is_empty() {
        bail!("{} names no compiler it can wrap", defn_path.display());
    }

    fs::write(&inject, inject_mk(&conf.cactup, conf_file, &wrapped))
        .with_context(|| format!("cannot write {}", inject.display()))?;
    for (path, text) in [
        (selftest_guarded(cc_dir), SELFTEST_GUARDED),
        (selftest_unguarded(cc_dir), SELFTEST_UNGUARDED),
    ] {
        let dir = path.parent().context("a self-test makefile has no parent directory")?;
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        fs::write(&path, text).with_context(|| format!("cannot write {}", path.display()))?;
    }
    Ok(notes)
}

/// One compiler variable the fragment wraps.
struct Wrapped<'a> {
    var: &'a str,
    patterns: Vec<&'a str>,
    /// The configured compiler command, word by word (`nvcc
    /// --compiler-bindir /usr/bin/g++` is three).
    words: Vec<String>,
}

/// Why a compiler variable is left alone.
#[derive(Debug, PartialEq)]
enum Unwrapped {
    /// The configuration has no such compiler (no Fortran, no CUDA).
    Unset,
    Because(String),
}

/// Does `rules` (a configuration's `make.config.rules`) define a pattern
/// rule for `pattern`, e.g. `%.c.o: $(SRCDIR)/%.c`?
fn defines_rule(rules: &str, pattern: &str) -> bool {
    rules.lines().any(|line| {
        line.strip_prefix(pattern).is_some_and(|rest| rest.trim_start().starts_with(':'))
    })
}

/// The command `var` is set to in `defn` (a configuration's
/// `make.config.defn`), if it is one the wrapper can stand in front of
/// without changing what runs.
///
/// The wrapper receives the command as words on its own command line, so
/// only a command that *is* plain words qualifies: anything make or the
/// shell would still have to interpret (`$(...)`, quotes, an `A=b` prefix,
/// a glob) would mean something else once it is an argument.
fn compiler_command(defn: &str, var: &str) -> Result<Vec<String>, Unwrapped> {
    let mut value = None;
    for line in defn.lines() {
        let line = line.trim_start();
        let line = line.strip_prefix("export").map_or(line, |rest| {
            if rest.starts_with(char::is_whitespace) { rest.trim_start() } else { line }
        });
        let Some(rest) = line.strip_prefix(var) else { continue };
        let rest = rest.trim_start();
        // `CC = ...` only: `CCFLAGS = ...` is another variable, and `CC :=`,
        // `CC +=`, `CC ?=` are assignments whose result this reader cannot
        // tell without being make.
        if let Some(assigned) = rest.strip_prefix('=') {
            value = Some(Ok(assigned.trim()));
        } else if [":=", "::=", "+=", "?=", "!="].iter().any(|op| rest.starts_with(op)) {
            value = Some(Err(Unwrapped::Because(format!("{var} is not set by a plain `=`"))));
        }
    }
    let value = match value {
        None => return Err(Unwrapped::Unset),
        Some(Err(why)) => return Err(why),
        Some(Ok(value)) => value,
    };
    // Cactus writes `none` for a compiler the configuration does not have.
    if value.is_empty() || value == "none" {
        return Err(Unwrapped::Unset);
    }
    let plain = |b: u8| b.is_ascii_alphanumeric() || b"_-+./:,@%= \t".contains(&b);
    if !value.bytes().all(plain) {
        return Err(Unwrapped::Because(format!("`{value}` is more than a plain command")));
    }
    let words: Vec<String> = value.split_ascii_whitespace().map(str::to_owned).collect();
    if words[0].contains('=') {
        return Err(Unwrapped::Because(format!("`{value}` starts with a variable assignment")));
    }
    let program = words[0].rsplit('/').next().unwrap_or(&words[0]);
    if OTHER_WRAPPERS.contains(&program) {
        return Err(Unwrapped::Because(format!("it already runs through {program}")));
    }
    Ok(words)
}

/// Quote `s` as one shell word inside a makefile variable value: single
/// quotes for the shell, `$$` for make, and `\#` so make does not start a
/// comment.
fn make_sh_quote(s: &Path) -> String {
    let quoted = s.display().to_string().replace('\'', "'\\''");
    format!("'{}'", quoted.replace('$', "$$").replace('#', "\\#"))
}

/// The injection fragment. Every wrapped compile runs as
///
/// ```text
/// <cactup> __cc <conf> <VAR>:<n> <the n words of the configured command> <args…>
/// ```
///
/// `<VAR>:<n>` tells the wrapper which exported make variable it stands in
/// for: a thorn may reassign `CC` in its own `make.code.defn`, after this
/// fragment is read, and the wrapper finds that in its environment (see
/// `wrapper::Compiler`).
fn inject_mk(cactup: &Path, conf_file: &Path, wrapped: &[Wrapped]) -> String {
    let mut out = String::from(
        "# Written by cactup for one build attempt and read through MAKEFILES.\n\
         # It runs the compilers of Cactus's object rules through cactup's build cache.\n\
         cactup_cc_on = $(filter %/lib/make/make.subdir,$(MAKEFILE_LIST))\n\
         %.c.o: private CACTUP_CC_SELFTEST = $(if $(cactup_cc_on),wrapped,unguarded)\n",
    );
    let wrapper = format!("{} {WRAP_VERB} {}", make_sh_quote(cactup), make_sh_quote(conf_file));
    for Wrapped { var, patterns, words } in wrapped {
        let command = words.join(" ");
        let count = words.len();
        for pattern in patterns {
            out.push_str(&format!(
                "{pattern}: private {var} = $(if $(cactup_cc_on),{wrapper} {var}:{count} ){command}\n"
            ));
        }
    }
    out
}

/// Run under `MAKEFILES=<fragment>` as a file named `…/lib/make/make.subdir`:
/// an object target must see the wrapped value, its prerequisite and an
/// unrelated target must not, and the object recipe's *environment* must
/// still carry the global value (that is where the wrapper looks for a
/// thorn's own compiler). A make without `private` (before 3.82) parses
/// the fragment's lines as something else and fails the first check.
const SELFTEST_GUARDED: &str = "\
export CACTUP_CC_SELFTEST = global
.PHONY: all cactup-selftest.c.o cactup-selftest-dep cactup-selftest-other
all: cactup-selftest.c.o cactup-selftest-other
cactup-selftest.c.o: cactup-selftest-dep
\t@test '$(CACTUP_CC_SELFTEST)' = wrapped
\t@test \"$$CACTUP_CC_SELFTEST\" = global
cactup-selftest-dep cactup-selftest-other:
\t@test '$(CACTUP_CC_SELFTEST)' = global
";

/// Run the same way under any other name: the guard must be off.
const SELFTEST_UNGUARDED: &str = "\
.PHONY: cactup-selftest.c.o
cactup-selftest.c.o:
\t@test '$(CACTUP_CC_SELFTEST)' = unguarded
";

#[cfg(test)]
mod tests {
    use super::*;

    const DEFN: &str = "\
# Compiler/executable info
export SHELL       = /bin/bash
export CC          = gcc
export CXX         = /opt/gcc-13/bin/g++
export CUCC        = nvcc --compiler-bindir /usr/bin/g++
export F90         = none
export CCFLAGS     = -O2
export MAKE
";

    #[test]
    fn reads_the_configured_compilers() {
        assert_eq!(compiler_command(DEFN, "CC").unwrap(), ["gcc"]);
        assert_eq!(compiler_command(DEFN, "CXX").unwrap(), ["/opt/gcc-13/bin/g++"]);
        assert_eq!(
            compiler_command(DEFN, "CUCC").unwrap(),
            ["nvcc", "--compiler-bindir", "/usr/bin/g++"]
        );
        // `none` and an absent variable both mean "this configuration has no
        // such compiler" — nothing to say about either.
        assert_eq!(compiler_command(DEFN, "F90"), Err(Unwrapped::Unset));
        assert_eq!(compiler_command(DEFN, "F77"), Err(Unwrapped::Unset));
    }

    #[test]
    fn the_last_assignment_wins_as_it_does_in_make() {
        let defn = "export CC = gcc\nCC = clang\n";
        assert_eq!(compiler_command(defn, "CC").unwrap(), ["clang"]);
    }

    #[test]
    fn leaves_alone_what_it_cannot_reproduce_as_words() {
        for (value, expect) in [
            ("$(HOME)/bin/gcc", "more than a plain command"),
            ("gcc -DNAME=\"a b\"", "more than a plain command"),
            ("LANG=C gcc", "starts with a variable assignment"),
            ("ccache gcc", "already runs through ccache"),
            ("/usr/lib/ccache/sccache g++", "already runs through sccache"),
        ] {
            let defn = format!("export CC = {value}\n");
            match compiler_command(&defn, "CC") {
                Err(Unwrapped::Because(why)) => assert!(why.contains(expect), "{value}: {why}"),
                other => panic!("{value}: {other:?}"),
            }
        }
        let appended = "export CC = gcc\nCC += -m64\n";
        assert!(matches!(compiler_command(appended, "CC"), Err(Unwrapped::Because(_))));
        // A flag carrying `=` is fine; only the program word may not.
        assert_eq!(
            compiler_command("CC = hipcc --amdgpu-target=gfx90a\n", "CC").unwrap(),
            ["hipcc", "--amdgpu-target=gfx90a"]
        );
    }

    #[test]
    fn finds_pattern_rules_by_their_target() {
        let rules = "%.c.o: $(SRCDIR)/%.c\n\t$(COMPILE_C)\n%.cc.o : $(SRCDIR)/%.cc\n%.c.d: $(SRCDIR)/%.c\n";
        assert!(defines_rule(rules, "%.c.o"));
        assert!(defines_rule(rules, "%.cc.o"));
        assert!(!defines_rule(rules, "%.C.o"));
        assert!(!defines_rule(rules, "%.c"));
    }

    #[test]
    fn the_fragment_wraps_each_pattern_and_escapes_for_make_and_sh() {
        let wrapped = [
            Wrapped { var: "CC", patterns: vec!["%.c.o"], words: vec!["gcc".into()] },
            Wrapped {
                var: "CUCC",
                patterns: vec!["%.cu.o"],
                words: vec!["nvcc".into(), "--compiler-bindir".into(), "/usr/bin/g++".into()],
            },
        ];
        let text = inject_mk(Path::new("/opt/it's $5 #1/cactup-abc"), Path::new("/b/cc/config.toml"), &wrapped);
        let wrapper = r"'/opt/it'\''s $$5 \#1/cactup-abc' __cc '/b/cc/config.toml'";
        assert!(
            text.contains(&format!("%.c.o: private CC = $(if $(cactup_cc_on),{wrapper} CC:1 )gcc\n")),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "%.cu.o: private CUCC = $(if $(cactup_cc_on),{wrapper} CUCC:3 )nvcc --compiler-bindir /usr/bin/g++\n"
            )),
            "{text}"
        );
        assert!(text.contains("cactup_cc_on = $(filter %/lib/make/make.subdir,$(MAKEFILE_LIST))"), "{text}");
    }

    /// A configuration directory as the probe needs to find it.
    fn fake_build(tmp: &Path, defn: &str) -> PathBuf {
        let root = tmp.join("Cactus");
        let config = root.join("configs/sim");
        fs::create_dir_all(root.join("lib/make")).unwrap();
        fs::write(root.join(SUBDIR_MAKEFILE), "").unwrap();
        fs::create_dir_all(config.join("config-data")).unwrap();
        fs::write(config.join("config-data/make.config.defn"), defn).unwrap();
        fs::write(
            config.join("config-data/make.config.rules"),
            "%.c.o: $(SRCDIR)/%.c\n%.cc.o: $(SRCDIR)/%.cc\n%.cu.o: $(SRCDIR)/%.cu\n%.F90.o: $(SRCDIR)/%.F90\n",
        )
        .unwrap();
        let cactup = tmp.join("cactup-abc");
        fs::write(&cactup, "").unwrap();
        let cc = config.join(".cactup-builds/0000/cc");
        let settings = super::super::Settings { mode: Mode::Record, root: tmp.join("cache") };
        let inputs = super::super::StageInputs {
            cactup: cactup.to_str().unwrap(),
            cactus_root: &root,
            config_dir: &config,
            machine: "mel5",
            universe: None,
            build_env: "",
        };
        super::super::stage(&cc, &settings, &inputs).unwrap().unwrap();
        cc
    }

    #[test]
    fn writes_the_fragment_for_the_rules_the_configuration_has() {
        let tmp = tempfile::tempdir().unwrap();
        let cc = fake_build(tmp.path(), DEFN);
        let notes = probe(&conf_path(&cc)).unwrap();
        assert!(notes.is_empty(), "{notes:?}");
        let text = fs::read_to_string(inject_path(&cc)).unwrap();
        assert!(text.contains("%.c.o: private CC = "), "{text}");
        assert!(text.contains("%.cc.o: private CXX = "), "{text}");
        assert!(text.contains("%.cu.o: private CUCC = "), "{text}");
        // No rule for it in this configuration's rules file.
        assert!(!text.contains("%.cxx.o"), "{text}");
        // F90 = none.
        assert!(!text.contains("F90"), "{text}");
        assert!(selftest_guarded(&cc).is_file() && selftest_unguarded(&cc).is_file());
    }

    #[test]
    fn says_which_compiler_it_leaves_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let cc = fake_build(tmp.path(), "export CC = ccache gcc\nexport CXX = g++\n");
        let notes = probe(&conf_path(&cc)).unwrap();
        assert_eq!(notes, ["leaving CC unwrapped (it already runs through ccache)"]);
        let text = fs::read_to_string(inject_path(&cc)).unwrap();
        assert!(!text.contains("private CC ="), "{text}");
        assert!(text.contains("private CXX ="), "{text}");
    }

    #[test]
    fn declines_when_the_tree_is_not_the_one_it_knows() {
        let tmp = tempfile::tempdir().unwrap();
        let cc = fake_build(tmp.path(), DEFN);
        fs::remove_file(tmp.path().join("Cactus").join(SUBDIR_MAKEFILE)).unwrap();
        let err = format!("{:#}", probe(&conf_path(&cc)).unwrap_err());
        assert!(err.contains("make.subdir"), "{err}");
        assert!(!inject_path(&cc).exists());
    }

    #[test]
    fn declines_when_nothing_can_be_wrapped() {
        let tmp = tempfile::tempdir().unwrap();
        let cc = fake_build(tmp.path(), "export CC = ccache gcc\n");
        let err = format!("{:#}", probe(&conf_path(&cc)).unwrap_err());
        assert!(err.contains("names no compiler it can wrap"), "{err}");
    }
}
