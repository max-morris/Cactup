//! The probe (§18.3): run by the build script after `make <config>-config`,
//! in the very context the compiles will run in (compute node, container
//! universe), to decide whether the wrapper can be used there and to write
//! the makefile fragment that puts it in front of the compilers.
//!
//! The fragment redefines Cactus's compile recipes (`COMPILE_C`,
//! `COMPILE_CXX`, …), copied from the configuration's own
//! `make.config.rules` with the one compiler reference replaced:
//!
//! ```make
//! override define COMPILE_C
//! current_wd=`$(GET_WD)` ; cd $(SCRATCH_BUILD) ; $(call cactup_cc_run,$(CC)) $(CPPFLAGS) …
//! endef
//! ```
//!
//! It touches no compiler variable. `$(CC)` is expanded where it always
//! was, so whatever the makefiles make of it for one target — a thorn's own
//! `CC`, however it sets it — is what runs; and nothing changes in any
//! recipe's environment, so an ExternalLibraries `build.sh` reading `$CC`
//! gets what it always got. (Setting the compiler variables per target
//! would do neither: before GNU make 4.4 a target-specific value of an
//! exported variable is exported into the recipes of the target's
//! prerequisites, `private` or not. And a `make CC=…` override is exported
//! into every recipe by design.)
//!
//! The redefinition happens only in Cactus's object sub-makes, decided when
//! the fragment is read (see [`inject_mk`]); everywhere else `make` reads it
//! through the inherited `MAKEFILES` it defines nothing, and it takes its
//! own name back out of `MAKEFILE_LIST`.

use super::{conf_path, inject_path, selftest_dir, BuildConf, PROBE_DECLINED, WRAP_VERB};
use crate::Res;
use anyhow::{bail, Context};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

/// Cactus's compile recipes (`lib/make/make.config.rules.in`), each with the
/// compiler variable its command starts with. A recipe is wrapped only if
/// the configuration's own rules file defines it with exactly one reference
/// to that variable — a flesh that reshapes a recipe loses the cache for
/// that language, not its build.
const RECIPES: &[(&str, &str)] = &[
    ("COMPILE_C", "CC"),
    ("COMPILE_CXX", "CXX"),
    ("COMPILE_CU", "CUCC"),
    ("COMPILE_F77", "F77"),
    ("COMPILE_F", "F90"),
    ("COMPILE_F90", "F90"),
];

/// The compiler text the self-test's recipes run. No program has this name;
/// the wrapper knows it (`wrapper::SELFTEST_COMPILER`) and answers whether it
/// could have wrapped a real compile here.
pub const SELFTEST_COMPILER: &str = "cactup:selftest";

/// The self-test makefile run from the configuration's `build` directory,
/// as an object sub-make is: the fragment must take effect.
pub fn selftest_wrapped(cc_dir: &Path) -> PathBuf {
    selftest_dir(cc_dir).join("wrapped.mk")
}

/// The self-test makefile run from anywhere else, as a third-party build
/// below `make <config>` is: the fragment must change nothing.
pub fn selftest_untouched(cc_dir: &Path) -> PathBuf {
    selftest_dir(cc_dir).join("untouched.mk")
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
        Ok(()) => 0,
        Err(e) => {
            eprintln!("cactup: build cache off for this build: {e:#}");
            PROBE_DECLINED
        }
    }
}

fn probe(conf_file: &Path) -> Res<()> {
    let conf = BuildConf::load(conf_file)?;
    let cc_dir = conf_file.parent().context("the configuration file has no parent directory")?;
    if conf_path(cc_dir) != conf_file {
        bail!("{} is not a build attempt's cache configuration", conf_file.display());
    }
    if !conf.cactup.is_file() {
        bail!("{} is not visible here", conf.cactup.display());
    }
    // Where Cactus's object sub-makes run. After a `realclean` it is not
    // there yet (make recreates it as it goes), and the self-test runs in
    // it. make compares its own working directory against this, and make's
    // is the physical one.
    let build_dir = conf.config_dir.join("build");
    fs::create_dir_all(&build_dir).with_context(|| format!("cannot create {}", build_dir.display()))?;
    let build_dir =
        fs::canonicalize(&build_dir).with_context(|| format!("cannot resolve {}", build_dir.display()))?;
    for path in [&conf.cactup, cc_dir, &build_dir] {
        carried_by_make(path)?;
    }

    let rules_path = conf.config_dir.join("config-data/make.config.rules");
    let rules = fs::read_to_string(&rules_path).with_context(|| format!("cannot read {}", rules_path.display()))?;
    let wrapped: Vec<Wrapped> = RECIPES
        .iter()
        .filter_map(|(recipe, var)| {
            let body = wrap_recipe(define_body(&rules, recipe)?, var)?;
            Some(Wrapped { recipe, body })
        })
        .collect();
    if wrapped.is_empty() {
        bail!("{} defines no compile recipe cactup knows how to wrap", rules_path.display());
    }

    let write = |path: PathBuf, text: String| {
        fs::write(&path, text).with_context(|| format!("cannot write {}", path.display()))
    };
    let selftest = selftest_dir(cc_dir);
    fs::create_dir_all(&selftest).with_context(|| format!("cannot create {}", selftest.display()))?;
    write(inject_path(cc_dir), inject_mk(&conf.cactup, conf_file, &build_dir, &wrapped))?;
    write(selftest_wrapped(cc_dir), selftest_wrapped_mk(&wrapped))?;
    write(selftest_untouched(cc_dir), SELFTEST_UNTOUCHED.to_owned())?;
    Ok(())
}

/// One recipe the fragment redefines.
struct Wrapped {
    recipe: &'static str,
    /// The recipe's lines with the compiler reference replaced.
    body: String,
}

/// The fragment names these paths in makefile text and in `MAKEFILES`, a
/// whitespace-separated list that make also expands. Rather than escape for
/// every context, refuse a path with anything in it but the characters that
/// mean nothing to make or the shell: installation paths are plain in
/// practice, and a refusal costs only the cache.
fn carried_by_make(path: &Path) -> Res<()> {
    let plain = |b: u8| b.is_ascii_alphanumeric() || b"/._-+:@=~".contains(&b);
    match path.to_str() {
        Some(text) if text.bytes().all(plain) => Ok(()),
        _ => bail!(
            "the path {} has a character in it that cactup does not pass through make \
             (letters, digits and / . _ - + : @ = ~ are fine)",
            path.display()
        ),
    }
}

/// The lines between `define <name>` and its `endef` in `rules` (a
/// configuration's `make.config.rules`), if it is defined exactly once and
/// plainly.
fn define_body<'a>(rules: &'a str, name: &str) -> Option<&'a str> {
    let mut bodies = Vec::new();
    let mut lines = rules.split_inclusive('\n');
    let mut offset = 0;
    while let Some(line) = lines.next() {
        offset += line.len();
        if line.trim_end() != format!("define {name}") {
            continue;
        }
        let start = offset;
        let mut end = None;
        for line in lines.by_ref() {
            match line.trim() {
                "endef" => {
                    end = Some(offset);
                    offset += line.len();
                    break;
                }
                // A nested `define`: not a shape this reader takes apart.
                inner if inner.starts_with("define ") => return None,
                _ => offset += line.len(),
            }
        }
        bodies.push(rules[start..end?].trim_end_matches('\n'));
    }
    match bodies.as_slice() {
        [body] if !body.is_empty() => Some(body),
        _ => None,
    }
}

/// `body` with its one `$(VAR)` replaced by the wrapper call; `None` if the
/// compiler is referred to more than once or not at all.
fn wrap_recipe(body: &str, var: &str) -> Option<String> {
    let reference = format!("$({var})");
    (body.matches(&reference).count() == 1)
        .then(|| body.replacen(&reference, &format!("$(call cactup_cc_run,{reference})"), 1))
}

/// The injection fragment.
///
/// It is read, through `MAKEFILES`, by every `make` below `make <config>`:
/// Cactus's own recursion, and any third-party build an ExternalLibraries
/// thorn starts. It acts only in the sub-makes that compile Cactus's
/// objects, which it recognizes when it is read:
///
/// - `CCTK_TARGET` is set — `make.thornlib` passes it to the `make.subdir`
///   sub-make on the command line — and
/// - the working directory is under this configuration's `build/`, where
///   those sub-makes run (a third-party build below one of them inherits
///   `CCTK_TARGET` through `MAKEFLAGS`, but runs in its own build tree), and
/// - the thorn's own make fragments do not mention the compile recipes: a
///   thorn that defines its own `COMPILE_C` in `make.code.deps` keeps it,
///   where `override` would silently win.
///
/// There it redefines the recipes so that the compiler runs as
///
/// ```text
/// CACTUP_CC_CMD='<what $(CC) expands to>' CACTUP_CC_SHELL='<$(SHELL)>' <cactup> __cc <conf> <args…>
/// ```
///
/// and stops handing itself on (`unexport MAKEFILES`): what an object
/// sub-make starts is not Cactus's make any more. The compiler text travels
/// as a quoted environment value, not as words of the command, so that the
/// shell does not take it apart before the wrapper has seen whether it is a
/// plain command (see `wrapper::Compiler`).
fn inject_mk(cactup: &Path, conf_file: &Path, build_dir: &Path, wrapped: &[Wrapped]) -> String {
    let (cactup, conf_file, build_dir) = (cactup.display(), conf_file.display(), build_dir.display());
    let mut out = format!(
        "# Written by cactup for one build attempt and read through MAKEFILES.\n\
         # It runs the compilers of Cactus's object rules through cactup's build cache.\n\
         ifdef CCTK_TARGET\n\
         ifneq ($(findstring |{build_dir}/,|$(CURDIR)/),)\n\
         unexport MAKEFILES\n\
         ifeq ($(findstring COMPILE_,$(shell cat '$(SRCDIR)/make.code.defn' '$(SRCDIR)/make.code.deps' 2>/dev/null)),)\n\
         define cactup_cc_run\n\
         CACTUP_CC_CMD='$(subst ','\\'',$1)' CACTUP_CC_SHELL='$(subst ','\\'',$(SHELL))' '{cactup}' {WRAP_VERB} '{conf_file}'\n\
         endef\n"
    );
    for Wrapped { recipe, body } in wrapped {
        out.push_str(&format!("override define {recipe}\n{body}\nendef\n"));
    }
    out.push_str(
        "endif\n\
         endif\n\
         endif\n\
         # A makefile that finds itself by $(firstword $(MAKEFILE_LIST)) must not find this one.\n\
         MAKEFILE_LIST := $(filter-out $(lastword $(MAKEFILE_LIST)),$(MAKEFILE_LIST))\n",
    );
    out
}

/// The self-test for the sub-makes the fragment is for. The build script
/// runs it from the configuration's `build` directory with `CCTK_TARGET`
/// set, under `MAKEFILES=<fragment>`, with the build's own `make`.
///
/// It defines each recipe the way `make.config.rules` does — after the
/// fragment, plainly — as a command that fails, and then runs it. So it
/// passes only if this make lets the fragment's definition win, hides the
/// fragment from `MAKEFILE_LIST` and from child makes, and the *real*
/// wrapped recipe text, quoting and all, reaches a cactup that can read its
/// configuration from here ([`SELFTEST_COMPILER`]).
fn selftest_wrapped_mk(wrapped: &[Wrapped]) -> String {
    let recipes: Vec<&str> = wrapped.iter().map(|w| w.recipe).collect();
    let mut out = format!(
        "GET_WD = pwd\n\
         SCRATCH_BUILD = .\n\
         CC = {SELFTEST_COMPILER}\n\
         CXX = $(CC)\n\
         CUCC = $(CC)\n\
         F77 = $(CC)\n\
         F90 = $(CC)\n"
    );
    for recipe in &recipes {
        out.push_str(&format!("define {recipe}\nexit 1\nendef\n"));
    }
    out.push_str(&format!(
        ".PHONY: all {names}\n\
         all: {names}\n\
         {names}:\n\
         \t@test '$(origin $@)' = override\n\
         \t@test '$(notdir $(firstword $(MAKEFILE_LIST)))' = wrapped.mk\n\
         \t@test -z \"$$MAKEFILES\"\n\
         \t@$($@)\n",
        names = recipes.join(" "),
    ));
    out
}

/// The self-test for every other make that reads the fragment: run from the
/// attempt's own directory, it must find its recipe as it defined it and
/// itself first in `MAKEFILE_LIST`.
const SELFTEST_UNTOUCHED: &str = "\
define COMPILE_C
exit 1
endef
.PHONY: all
all:
\t@test '$(origin COMPILE_C)' = file
\t@test '$(origin cactup_cc_run)' = undefined
\t@test '$(notdir $(firstword $(MAKEFILE_LIST)))' = untouched.mk
";

#[cfg(test)]
mod tests {
    use super::*;

    /// The two recipes as Cactus 4.20's `make.config.rules` has them.
    const RULES: &str = "\
define NOTIFY_COMPILING
\t@echo COMPILING $<
endef

# Define how to do a C compilation
define PREPROCESS_C
{ cat $<; } | $(PERL) -s $(C_FILE_PROCESSOR) $(CONFIG) > $(notdir $<)
endef

define COMPILE_C
current_wd=`$(GET_WD)` ; cd $(SCRATCH_BUILD) ; $(CC) $(CPPFLAGS) $(CFLAGS) $(CCOMPILEONLY)$(OPTIONSEP)$$current_wd$(DIRSEP)$@ $$current_wd$(DIRSEP)$(notdir $<) $(INCLUDE_LINE) $(EXTRA_DEFINES:%=-D%) -DCCODE
endef

define COMPILE_F90
current_wd=`$(GET_WD)` ; cd $(SCRATCH_BUILD) ; $(F90) $(F90FLAGS) $(INCLUDE_LINE_F) $(FCOMPILEONLY)$(OPTIONSEP)$$current_wd$(DIRSEP)$@ $$current_wd$(DIRSEP)$(basename $(notdir $<)).$(F90_SUFFIX)
endef

%.c.o: $(SRCDIR)/%.c
\t$(COMPILE_C)
";

    #[test]
    fn finds_a_recipe_by_its_define() {
        let body = define_body(RULES, "COMPILE_C").unwrap();
        assert!(body.starts_with("current_wd=`$(GET_WD)`") && body.ends_with("-DCCODE"), "{body}");
        assert!(define_body(RULES, "COMPILE_CXX").is_none());
        // `COMPILE_C` is not `COMPILE_CXX`, nor the other way round.
        assert!(define_body("define COMPILE_CXX\ng++\nendef\n", "COMPILE_C").is_none());
    }

    #[test]
    fn a_recipe_it_cannot_take_apart_is_left_alone() {
        // Defined twice: which one make ends up with is make's business.
        assert!(define_body("define COMPILE_C\na\nendef\ndefine COMPILE_C\nb\nendef\n", "COMPILE_C").is_none());
        // Never closed, empty, or holding a `define` of its own.
        assert!(define_body("define COMPILE_C\n$(CC) -c\n", "COMPILE_C").is_none());
        assert!(define_body("define COMPILE_C\nendef\n", "COMPILE_C").is_none());
        assert!(define_body("define COMPILE_C\ndefine X\nendef\nendef\n", "COMPILE_C").is_none());
    }

    #[test]
    fn replaces_the_one_compiler_reference() {
        let body = wrap_recipe("cd $(SCRATCH_BUILD) ; $(CC) $(CFLAGS) -c", "CC").unwrap();
        assert_eq!(body, "cd $(SCRATCH_BUILD) ; $(call cactup_cc_run,$(CC)) $(CFLAGS) -c");
        // `$(CXX)` and `$(CCOMPILEONLY)` are not references to `CC`.
        assert!(wrap_recipe("$(CXX) $(CCOMPILEONLY)", "CC").is_none());
        // Two references: this is not the recipe shape cactup knows.
        assert!(wrap_recipe("$(CC) -E x | $(CC) -c", "CC").is_none());
    }

    #[test]
    fn paths_make_would_mangle_are_refused() {
        assert!(carried_by_make(Path::new("/work/u1/et_2026-05/~x/Cactus@v2/configs/sim+debug=1:a")).is_ok());
        // A comma separates the arguments of a make function, a blank the
        // entries of MAKEFILES, and so on.
        for bad in ["/a,b", "/work/my files/c", "/a$b", "/a#b", "/a(b)", "/it's", "/a%b", "/a\\b", "/a|b", "/caf\u{e9}"] {
            let err = carried_by_make(Path::new(bad)).unwrap_err().to_string();
            assert!(err.contains("does not pass through make"), "{bad}: {err}");
        }
    }

    /// A configured Cactus configuration as the probe needs to find it.
    /// Returns its attempt's `cc` directory.
    fn fake_build(tmp: &Path, rules: &str) -> PathBuf {
        let config = tmp.join("Cactus/configs/sim");
        // No `build/`: as after a `realclean`.
        fs::create_dir_all(config.join("config-data")).unwrap();
        fs::write(config.join("config-data/make.config.rules"), rules).unwrap();
        let cactup = tmp.join("cactup-abc");
        fs::write(&cactup, "").unwrap();
        let cc = config.join(".cactup-builds/0000/cc");
        super::super::stage(&cc, super::super::Mode::Record, cactup.to_str().unwrap(), &config).unwrap().unwrap();
        cc
    }

    #[test]
    fn writes_the_fragment_for_the_recipes_the_configuration_has() {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let cc = fake_build(&root, RULES);
        probe(&conf_path(&cc)).unwrap();

        let text = fs::read_to_string(inject_path(&cc)).unwrap();
        let build = root.join("Cactus/configs/sim/build");
        assert!(text.contains(&format!("ifneq ($(findstring |{}/,|$(CURDIR)/),)\n", build.display())), "{text}");
        assert!(
            text.contains(&format!(
                "define cactup_cc_run\nCACTUP_CC_CMD='$(subst ','\\'',$1)' CACTUP_CC_SHELL='$(subst ','\\'',$(SHELL))' \
                 '{}' __cc '{}'\nendef\n",
                root.join("cactup-abc").display(),
                conf_path(&cc).display()
            )),
            "{text}"
        );
        assert!(
            text.contains(
                "override define COMPILE_C\ncurrent_wd=`$(GET_WD)` ; cd $(SCRATCH_BUILD) ; \
                 $(call cactup_cc_run,$(CC)) $(CPPFLAGS) $(CFLAGS)"
            ),
            "{text}"
        );
        assert!(text.contains("override define COMPILE_F90\n"), "{text}");
        // Not in this configuration's rules file.
        assert!(!text.contains("COMPILE_CXX"), "{text}");
        assert!(text.trim_end().ends_with("MAKEFILE_LIST := $(filter-out $(lastword $(MAKEFILE_LIST)),$(MAKEFILE_LIST))"));

        let selftest = fs::read_to_string(selftest_wrapped(&cc)).unwrap();
        assert!(selftest.contains("all: COMPILE_C COMPILE_F90\n"), "{selftest}");
        assert!(selftest_untouched(&cc).is_file());
    }

    #[test]
    fn declines_when_the_rules_are_not_the_ones_it_knows() {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let cc = fake_build(&root, "%.c.o: $(SRCDIR)/%.c\n\t$(CC) -c $<\n");
        let err = format!("{:#}", probe(&conf_path(&cc)).unwrap_err());
        assert!(err.contains("defines no compile recipe cactup knows how to wrap"), "{err}");
        assert!(!inject_path(&cc).exists());
    }

    #[test]
    fn declines_before_the_configuration_is_configured() {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let cc = fake_build(&root, RULES);
        fs::remove_file(root.join("Cactus/configs/sim/config-data/make.config.rules")).unwrap();
        let err = format!("{:#}", probe(&conf_path(&cc)).unwrap_err());
        assert!(err.contains("cannot read") && err.contains("make.config.rules"), "{err}");
    }
}
