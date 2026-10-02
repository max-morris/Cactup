//! The probe (§18.3): run by the build script before `make <config>`, in
//! the very context the compiles will run in (compute node, container
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
//! the fragment is read (see [`inject_mk`]). Every other make that reads it
//! through the inherited `MAKEFILES` gets one empty rule, for the fragment
//! itself, and nothing else; and in every make the fragment takes its own
//! name back out of `MAKEFILE_LIST`.

use super::{conf_path, inject_path, selftest_dir, BuildConf, PROBE_DECLINED, WRAP_VERB};
use crate::Res;
use anyhow::{bail, Context};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

/// Cactus's compile recipes (`lib/make/make.config.rules.in`), each with the
/// compiler variable its command starts with. A recipe is wrapped only if
/// the configuration's own rules file has it in the shape cactup knows
/// ([`define_body`], [`wrap_recipe`]) — a flesh that reshapes a recipe loses
/// the cache for that language, not its build.
const RECIPES: &[(&str, &str)] = &[
    ("COMPILE_C", "CC"),
    ("COMPILE_CXX", "CXX"),
    ("COMPILE_CU", "CUCC"),
    ("COMPILE_F77", "F77"),
    ("COMPILE_F", "F90"),
    ("COMPILE_F90", "F90"),
];

/// The compiler text the self-test's recipes run. No program has this name;
/// the wrapper knows it and answers whether it could have wrapped a real
/// compile here.
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

/// The file a self-test run (`wrapped`, `untouched`, or `elsewhere`) leaves
/// behind when every one of its checks ran and passed. The build script takes the
/// file, not make's exit status alone, as the pass: a make that found
/// nothing to do also exits 0.
pub fn selftest_passed(cc_dir: &Path, which: &str) -> PathBuf {
    selftest_dir(cc_dir).join(format!("{which}.passed"))
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
    // Where Cactus's object sub-makes run. make compares its own working
    // directory against this, and make's is the physical one: `build` may
    // itself be a link to somewhere roomier. After a `realclean` it is not
    // there at all (make recreates it as it goes); it is created below, as
    // a plain directory.
    let build_dir = conf.config_dir.join("build");
    let build_dir = match fs::canonicalize(&build_dir) {
        Ok(physical) => physical,
        Err(_) => fs::canonicalize(&conf.config_dir)
            .with_context(|| format!("cannot resolve {}", conf.config_dir.display()))?
            .join("build"),
    };
    let selftest = selftest_dir(cc_dir);
    for path in [&conf.cactup, cc_dir, &build_dir, &selftest] {
        carried_by_make(path)?;
    }

    let rules_path = conf.config_dir.join("config-data/make.config.rules");
    let rules = fs::read_to_string(&rules_path).with_context(|| format!("cannot read {}", rules_path.display()))?;
    // A recipe redefined in a file the rules include would be one more
    // definition this reader does not see.
    if rules.lines().any(|line| matches!(line.split_whitespace().next(), Some("include" | "-include" | "sinclude"))) {
        bail!("{} includes other makefiles, which cactup does not follow", rules_path.display());
    }
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

    // Nothing is written before this point: a probe that declines leaves
    // the configuration as it found it.
    let write = |path: PathBuf, text: String| {
        fs::write(&path, text).with_context(|| format!("cannot write {}", path.display()))
    };
    fs::create_dir_all(&build_dir).with_context(|| format!("cannot create {}", build_dir.display()))?;
    // Fresh: what a self-test or a build left here before says nothing
    // about this one.
    let _ = fs::remove_dir_all(&selftest);
    let _ = fs::remove_file(super::events_path(cc_dir));
    fs::create_dir_all(&selftest).with_context(|| format!("cannot create {}", selftest.display()))?;
    write(inject_path(cc_dir), inject_mk(&conf.cactup, conf_file, &inject_path(cc_dir), &build_dir, &wrapped))?;
    write(selftest_wrapped(cc_dir), selftest_wrapped_mk(&wrapped, &selftest_passed(cc_dir, "wrapped")))?;
    write(selftest_untouched(cc_dir), selftest_untouched_mk(&selftest))?;
    Ok(())
}

/// One recipe the fragment redefines.
struct Wrapped {
    recipe: &'static str,
    /// The recipe's lines with the compiler reference replaced.
    body: String,
}

/// The fragment names these paths in makefile text — as a rule's target,
/// inside function calls, inside shell quotes — and in `MAKEFILES`, a
/// whitespace-separated list that make also expands. Rather than escape for
/// every one of those contexts, refuse a path with anything in it but the
/// characters that mean nothing in any of them: installation paths are
/// plain in practice, and a refusal costs only the cache.
fn carried_by_make(path: &Path) -> Res<()> {
    let plain = |b: u8| b.is_ascii_alphanumeric() || b"/._-+@~".contains(&b);
    match path.to_str() {
        Some(text) if text.bytes().all(plain) => Ok(()),
        _ => bail!(
            "the path {} has a character in it that cactup does not pass through make \
             (letters, digits and / . _ - + @ ~ are fine)",
            path.display()
        ),
    }
}

/// Does `line` (trimmed) open a `define`, with or without `override` and
/// the like in front?
fn opens_define(line: &str) -> bool {
    let mut rest = line;
    while let Some(after) = ["override", "export", "private"]
        .iter()
        .find_map(|word| rest.strip_prefix(word).filter(|after| after.starts_with(char::is_whitespace)))
    {
        rest = after.trim_start();
    }
    rest.strip_prefix("define").is_some_and(|after| after.starts_with(char::is_whitespace))
}

/// Does `line` (trimmed) define the make variable `name` in some way:
/// `NAME = …`, `NAME := …`, `define NAME`, with or without `override` and
/// the like in front?
fn defines(line: &str, name: &str) -> bool {
    let mut rest = line;
    let mut by_define = false;
    while let Some((word, after)) = ["override", "export", "private", "define"]
        .iter()
        .find_map(|word| Some((*word, rest.strip_prefix(word).filter(|after| after.starts_with(char::is_whitespace))?)))
    {
        by_define |= word == "define";
        rest = after.trim_start();
    }
    // `COMPILE_CXX` is not `COMPILE_C`: the name has to end where it ends.
    let Some(after) = rest.strip_prefix(name).filter(|after| !after.starts_with(|c: char| c.is_alphanumeric() || c == '_'))
    else {
        return false;
    };
    let after = after.trim_start();
    // After `define` (or `export`), the name is all it takes; whatever
    // follows it is an operator or a comment. A bare assignment goes on
    // with an operator.
    by_define || (after.is_empty() && rest.len() < line.len()) || ["=", ":=", "::=", "+=", "?=", "!="].iter().any(|op| after.starts_with(op))
}

/// The lines between `define <name>` and its `endef` in `rules` (a
/// configuration's `make.config.rules`) — if that is the one and only way
/// the file defines `name`, plainly and unconditionally. Anything else
/// (a second definition, an assignment, a definition inside a conditional)
/// means the body here may not be the one make ends up with.
fn define_body<'a>(rules: &'a str, name: &str) -> Option<&'a str> {
    let mut found = None;
    let mut conditionals = 0usize;
    let mut lines = rules.split_inclusive('\n');
    let mut offset = 0;
    while let Some(line) = lines.next() {
        offset += line.len();
        let text = line.trim();
        match text.split_whitespace().next() {
            Some("ifeq" | "ifneq" | "ifdef" | "ifndef") => conditionals += 1,
            Some("endif") => conditionals = conditionals.saturating_sub(1),
            _ => {}
        }
        let ours = defines(text, name);
        if ours && (text != format!("define {name}") || conditionals != 0 || found.is_some()) {
            return None;
        }
        if !opens_define(text) {
            continue;
        }
        // Inside a `define`, ours or another: nothing in there is makefile
        // syntax to this reader until the `endef`.
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
        let body = rules[start..end?].trim_end_matches('\n');
        if ours {
            found = Some(body);
        }
    }
    found.filter(|body| !body.is_empty())
}

/// `body` with its one `$(VAR)` replaced by the wrapper call — if that
/// reference is the command of a simple command: at the start of a recipe
/// line or right after `;`, `&&` or `||`, outside any quotes, and a word of
/// its own. A compiler that is referred to more than once, or behind
/// something else (`nice $(CC)`, `X=$(CC)`, `echo "…; $(CC)"`), is not a
/// shape cactup stands in front of.
fn wrap_recipe(body: &str, var: &str) -> Option<String> {
    let reference = format!("$({var})");
    let at = body.find(&reference)?;
    let (before, after) = (&body[..at], &body[at + reference.len()..]);
    if after.contains(&reference) {
        return None;
    }
    // The logical line the reference is on: a line that ends in `\` goes on
    // in the next. So `… ; \` followed by `$(CC)` has the compiler after
    // the `;`, and `nice \` followed by `$(CC)` has it after `nice`.
    let joined = before.replace("\\\n", " ");
    let line = joined.rsplit('\n').next().unwrap_or_default().trim_end();
    let starts_line = line.trim_start_matches(['@', '-', '+', ' ', '\t']).is_empty();
    let starts_command = starts_line || [";", "&&", "||"].iter().any(|sep| line.ends_with(sep));
    let quoted = ['\'', '"', '`'].iter().any(|quote| before.matches(*quote).count() % 2 == 1);
    let own_word = after.is_empty() || after.starts_with([' ', '\t', '\n']);
    (starts_command && !quoted && own_word).then(|| format!("{before}$(call cactup_cc_run,{reference}){after}"))
}

/// The injection fragment.
///
/// It is read, through `MAKEFILES`, by every `make` below `make <config>`:
/// Cactus's own recursion, and any third-party build started along the
/// way. It acts only in the sub-makes that compile Cactus's objects, which
/// it recognizes when it is read:
///
/// - `CCTK_TARGET` is set — `make.thornlib` passes it to the `make.subdir`
///   sub-make on the command line — and
/// - the working directory is under this configuration's `build/`, where
///   those sub-makes run (a third-party build below one of them inherits
///   `CCTK_TARGET` through `MAKEFLAGS`, but runs in its own build tree), and
/// - the thorn's own make fragments neither mention the compile recipes nor
///   could define one out of sight ([`STAND_DOWN`]): a thorn that defines
///   its own `COMPILE_C` in `make.code.deps` keeps it, where `override`
///   would silently win. `grep` reads the two files, and the fragment wraps
///   only on its "no match": no file, an unreadable one, or no `grep`
///   leaves the thorn to plain make.
///
/// There it redefines the recipes so that the compiler runs as
///
/// ```text
/// <cactup> __cc <conf> '<what $(CC) expands to>' '<$(SHELL)>' <args…>
/// ```
///
/// and takes itself out of `MAKEFILES`, so that nothing an object sub-make
/// starts reads it. The compiler text travels as one quoted argument, not
/// as words of the command, so that the shell does not take it apart before
/// the wrapper has seen whether it is a plain command (see `wrapper::Job`).
///
/// Everywhere, it gives itself an empty rule — make tries to remake every
/// makefile it reads, and a foreign makefile's match-anything rule would
/// otherwise be run for it — and removes itself from `MAKEFILE_LIST`, for
/// makefiles that find themselves by `$(firstword $(MAKEFILE_LIST))`.
/// What in a thorn's `make.code.defn` or `make.code.deps` makes the fragment
/// stand down for it (§18.3), as a POSIX extended regular expression: any
/// mention of the compile recipes, and the directives by which a recipe
/// could be defined where a reading of these two files does not see it —
/// an `include` (`-include`, `sinclude`), a `define` (behind `override`,
/// `export`, `private` or `unexport` too), a `load` (`-load`) of a make
/// plugin, and `$(eval` and `$(guile` (or with braces). Directives, not
/// words: `INCLUDE_DIRS`, or a comment that says "include", is no reason.
/// make joins a line ending in `\` to the next with a space before it reads
/// a directive, so a `\` counts as the space after the keyword. A line that
/// merely continues the one before and begins with `include` is matched all
/// the same; that costs the thorn the cache, never its recipe.
///
/// It goes into the fragment inside `$(shell …)` and single quotes, so it
/// has no `'`, no `#` (make's comment) and no unbalanced parenthesis.
const STAND_DOWN: &str = "COMPILE_\
    |^[[:space:]]*(-|s)?include([[:space:]]|\\\\|$)\
    |^[[:space:]]*((override|export|private|unexport)([[:space:]]|\\\\)+)*define([[:space:]]|\\\\|$)\
    |^[[:space:]]*-?load([[:space:]]|\\\\|$)\
    |[$].(eval|guile)([[:space:]]|\\\\|$)";

fn inject_mk(cactup: &Path, conf_file: &Path, inject: &Path, build_dir: &Path, wrapped: &[Wrapped]) -> String {
    let (cactup, conf_file, inject, build_dir) =
        (cactup.display(), conf_file.display(), inject.display(), build_dir.display());
    // Each of the two files that is there, and `/dev/null` so that `grep`
    // never reads its standard input. (Not by `$(wildcard …)`: GNU make
    // 4.2.1 built against a current C library crashes in it.) For make, a
    // `$` is `$$`.
    let scan = format!(
        "for f in '$(SRCDIR)/make.code.defn' '$(SRCDIR)/make.code.deps'; do [ -e \"$$f\" ] && set -- \"$$@\" \"$$f\"; done; \
         grep -Eq -e '{}' /dev/null \"$$@\" 2>/dev/null",
        STAND_DOWN.replace('$', "$$")
    );
    let mut out = format!(
        "# Written by cactup for one build attempt and read through MAKEFILES.\n\
         # It runs the compilers of Cactus's object rules through cactup's build cache.\n\
         {inject}: ;\n\
         ifdef CCTK_TARGET\n\
         ifneq ($(findstring |{build_dir}/,|$(CURDIR)/),)\n\
         MAKEFILES := $(filter-out {inject},$(MAKEFILES))\n\
         ifeq ($(shell {scan}; echo $$?),1)\n\
         define cactup_cc_run\n\
         '{cactup}' {WRAP_VERB} '{conf_file}' '$(subst ','\\'',$1)' '$(subst ','\\'',$(SHELL))'\n\
         endef\n"
    );
    for Wrapped { recipe, body } in wrapped {
        out.push_str(&format!("override define {recipe}\n{body}\nendef\n"));
    }
    out.push_str(
        "endif\n\
         endif\n\
         endif\n\
         MAKEFILE_LIST := $(filter-out $(lastword $(MAKEFILE_LIST)),$(MAKEFILE_LIST))\n",
    );
    out
}

/// The self-test for the sub-makes the fragment is for. The build script
/// runs its `all` from the configuration's `build` directory with
/// `CCTK_TARGET` set, under `MAKEFILES=<fragment>`, with the build's own
/// `make`.
///
/// It defines each recipe the way `make.config.rules` does — after the
/// fragment, plainly — as a command that fails, and then runs it. So it
/// passes (and writes `passed`) only if this make lets the fragment's
/// definition win, hides the fragment from `MAKEFILE_LIST` and from child
/// makes, and the *real* wrapped recipe text, quoting and all, reaches a
/// cactup that can wrap a compile from here ([`SELFTEST_COMPILER`]).
///
/// Every check of a recipe is one shell command chained with `&&`, ending
/// in the recipe's own marker file, and `passed` is written only when
/// every marker is there: a make told to carry on past errors (`-i`, `-k`)
/// leaves markers out, not a pass behind.
fn selftest_wrapped_mk(wrapped: &[Wrapped], passed: &Path) -> String {
    let recipes: Vec<&str> = wrapped.iter().map(|w| w.recipe).collect();
    let selftest = passed.parent().unwrap_or(Path::new(".")).display();
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
    let markers: Vec<String> = recipes.iter().map(|recipe| format!("test -e '{selftest}/wrapped.{recipe}'")).collect();
    out.push_str(&format!(
        ".PHONY: all {names}\n\
         all: {names}\n\
         \t@{markers} && : > '{passed}'\n\
         {names}:\n\
         \t@test '$(origin $@)' = override && \\\n\
         \t test '$(notdir $(firstword $(MAKEFILE_LIST)))' = wrapped.mk && \\\n\
         \t test -z \"$$MAKEFILES\" && \\\n\
         \t {{ $($@) ; }} && \\\n\
         \t : > '{selftest}/wrapped.$@'\n",
        names = recipes.join(" "),
        markers = markers.join(" && "),
        passed = passed.display(),
    ));
    out
}

/// The self-test for every other make that reads the fragment. The build
/// script runs its `all` twice: from the attempt's own directory with
/// `CCTK_TARGET` set (a third-party build below an object sub-make inherits
/// it), and from the configuration's `build` directory without it (anything
/// else Cactus starts there). Each run passes (and writes its `passed`
/// file, named by `RUN`) only if it finds its recipe as it defined it,
/// nothing of the fragment's defined, and itself first in `MAKEFILE_LIST`.
///
/// Its match-anything rule stands for a forwarding makefile's. make tries
/// to remake every makefile it reads, before anything else, and the rule
/// must not be run for the fragment: it leaves `remade` behind if it is,
/// for `all` to find (make itself shrugs off a makefile it failed to
/// remake).
fn selftest_untouched_mk(selftest: &Path) -> String {
    format!(
        "$(lastword $(MAKEFILE_LIST)): ;\n\
         define COMPILE_C\n\
         exit 1\n\
         endef\n\
         .PHONY: all cactup-selftest-force\n\
         all:\n\
         \t@test '$(origin COMPILE_C)' = file && \\\n\
         \t test '$(origin cactup_cc_run)' = undefined && \\\n\
         \t test '$(notdir $(firstword $(MAKEFILE_LIST)))' = untouched.mk && \\\n\
         \t test ! -e '{remade}' && \\\n\
         \t : > '{selftest}/$(RUN).passed'\n\
         cactup-selftest-force: ;\n\
         %: cactup-selftest-force\n\
         \t@: > '{remade}'\n",
        remade = selftest.join("remade").display(),
        selftest = selftest.display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two recipes as Cactus 4.20's `make.config.rules` has them.
    const RULES: &str = "\
define NOTIFY_COMPILING
\t@echo COMPILING $<
endef

ifeq ($(strip $(PERL_BACKUP_NECESSARY)),)
define DEPENDENCY_FIXER
\t$(PERL) -pi -e 's{x}{y}' $@
endef
else
define DEPENDENCY_FIXER
\t$(PERL) -pi.bak -e 's{x}{y}' $@
endef
endif

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

    /// The stand-down pattern, as `grep -E` reads it: what defines or could
    /// define a recipe out of sight is found, words that only look like it
    /// are not.
    #[test]
    fn the_stand_down_pattern_finds_directives_not_words() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("make.code.deps");
        let matches = |text: &str| {
            fs::write(&file, text).unwrap();
            let status = std::process::Command::new("grep").args(["-Eq", "-e", STAND_DOWN, "/dev/null"]).arg(&file).status().unwrap();
            match status.code() {
                Some(0) => true,
                Some(1) => false,
                other => panic!("grep failed ({other:?}) on {text:?}"),
            }
        };
        for found in [
            "COMPILE_C = $(CC) -c\n",
            "SRCS = a.c\n  $(COMPILE_CXX)\n",
            "include extra.mk\n",
            "  -include $(SRCDIR)/more.mk\n",
            "sinclude x.mk\n",
            "\tinclude x.mk\n",
            "include\n",
            "define RECIPE\nx\nendef\n",
            "override define RECIPE\nendef\n",
            "export  override define RECIPE =\nendef\n",
            "private define X\nendef\n",
            "$(eval X := 1)\n",
            "${eval X := 1}\n",
            "FOO := $(foreach t,a b,$(eval $(t)_y := 1))\n",
            // What make reads as a directive once it has joined the line
            // to the next.
            "include\\\nextra.mk\n",
            "-include\\\n  extra.mk\n",
            "define\\\nRECIPE\nendef\n",
            "override\\\ndefine RECIPE\nendef\n",
            "X := $(eval\\\n$(file <extra.mk))\n",
            "load plugin.so\n",
            "-load\\\nplugin.so\n",
            "$(guile (gmk-eval \"X = 1\"))\n",
        ] {
            assert!(matches(found), "{found:?}");
        }
        for ignored in [
            "SRCS = a.c b.cc\n",
            "INCLUDE_DIRS += $(SRCDIR)/include\n",
            "# include the generated header\n",
            "CXXFLAGS += -include config.h\n",
            "undefine X\n",
            "redefine = no\n",
            "defines := -DX\n",
            "X = $(evaluate)\n",
            "loaded = yes\n",
            "\n",
        ] {
            assert!(!matches(ignored), "{ignored:?}");
        }
    }

    #[test]
    fn finds_a_recipe_by_its_define() {
        let body = define_body(RULES, "COMPILE_C").unwrap();
        assert!(body.starts_with("current_wd=`$(GET_WD)`") && body.ends_with("-DCCODE"), "{body}");
        assert!(define_body(RULES, "COMPILE_CXX").is_none());
        // `COMPILE_C` is not `COMPILE_CXX`, nor the other way round.
        assert!(define_body("define COMPILE_CXX\ng++\nendef\n", "COMPILE_C").is_none());
        assert!(define_body("COMPILE_CXX = g++\ndefine COMPILE_C\ngcc\nendef\n", "COMPILE_C").is_some());
    }

    #[test]
    fn a_recipe_make_might_not_end_up_with_is_left_alone() {
        for (rules, why) in [
            ("define COMPILE_C\na\nendef\ndefine COMPILE_C\nb\nendef\n", "defined twice"),
            ("define COMPILE_C\n$(CC) -c\nendef\nCOMPILE_C += -g\n", "appended to"),
            ("COMPILE_C = $(CC) -c\ndefine COMPILE_C\n$(CC) -c\nendef\n", "also assigned"),
            ("define COMPILE_C\n$(CC) -c\nendef\noverride define COMPILE_C\nx\nendef\n", "overridden"),
            ("define COMPILE_C =\n$(CC) -c\nendef\n", "defined with an operator"),
            ("define COMPILE_C\n$(CC) -c\nendef\ndefine COMPILE_C # again\nx\nendef\n", "defined again, with a comment"),
            ("define COMPILE_C # the C recipe\n$(CC) -c\nendef\n", "defined with a comment after the name"),
            ("ifeq ($(A),b)\ndefine COMPILE_C\n$(CC) -c\nendef\nendif\n", "defined conditionally"),
            ("ifdef A\nelse\ndefine COMPILE_C\n$(CC) -c\nendef\nendif\n", "defined in an else branch"),
            ("define COMPILE_C\n$(CC) -c\n", "never closed"),
            ("define COMPILE_C\nendef\n", "empty"),
            ("define COMPILE_C\ndefine X\nendef\nendef\n", "holding a define of its own"),
        ] {
            assert!(define_body(rules, "COMPILE_C").is_none(), "{why}");
        }
        // What another `define` holds is text, not definitions.
        let quoted = "define HELP\nCOMPILE_C = how to compile\nifeq is a conditional\nendef\ndefine COMPILE_C\n$(CC) -c\nendef\n";
        assert_eq!(define_body(quoted, "COMPILE_C"), Some("$(CC) -c"));
    }

    #[test]
    fn replaces_the_one_compiler_reference_in_command_position() {
        for (body, wrapped) in [
            ("$(CC) -c", "$(call cactup_cc_run,$(CC)) -c"),
            ("cd $(SCRATCH_BUILD) ; $(CC) $(CFLAGS) -c", "cd $(SCRATCH_BUILD) ; $(call cactup_cc_run,$(CC)) $(CFLAGS) -c"),
            ("cd x && $(CC) -c", "cd x && $(call cactup_cc_run,$(CC)) -c"),
            ("test -d x || $(CC)", "test -d x || $(call cactup_cc_run,$(CC))"),
            ("echo compiling\n\t@$(CC) -c", "echo compiling\n\t@$(call cactup_cc_run,$(CC)) -c"),
            ("cd x ; \\\n\t$(CC) -c", "cd x ; \\\n\t$(call cactup_cc_run,$(CC)) -c"),
        ] {
            assert_eq!(wrap_recipe(body, "CC").as_deref(), Some(wrapped), "{body}");
        }
        for body in [
            // `$(CXX)` and `$(CCOMPILEONLY)` are not references to `CC`.
            "$(CXX) $(CCOMPILEONLY)",
            // Two references.
            "$(CC) -E x | $(CC) -c",
            // Not the command: something else is.
            "cd x ; $(LAUNCHER) $(CC) -c",
            "nice $(CC) -c",
            "X=$(CC) ./compile",
            "echo $(CC)",
            "cat x | $(CC) -c",
            // Part of a longer word.
            "$(CC)-13 -c",
            // Inside quotes, or a command substitution: text, not a command.
            "echo \"compiling; $(CC) -c\"",
            "echo 'x' ; echo 'y; $(CC)'",
            "v=`cd x && $(CC) --version`",
            // Continuing the line above.
            "nice \\\n\t$(CC) -c",
        ] {
            assert_eq!(wrap_recipe(body, "CC"), None, "{body}");
        }
    }

    #[test]
    fn recognizes_definitions_of_a_variable() {
        for line in [
            "define COMPILE_C",
            "COMPILE_C = x",
            "COMPILE_C:=x",
            "COMPILE_C += x",
            "override COMPILE_C ?= x",
            "export override define COMPILE_C",
            "define COMPILE_C :=",
            "define COMPILE_C # a comment",
            "export COMPILE_C",
        ] {
            assert!(defines(line, "COMPILE_C"), "{line}");
        }
        for line in ["COMPILE_CXX = x", "define COMPILE_CXX", "\t$(COMPILE_C)", "COMPILE_C", "%.o: COMPILE_C = x", "# COMPILE_C = x", "undefine COMPILE_C"] {
            assert!(!defines(line, "COMPILE_C"), "{line}");
        }
    }

    #[test]
    fn paths_make_would_mangle_are_refused() {
        assert!(carried_by_make(Path::new("/work/u1/et_2026-05/~x/Cactus@v2/configs/sim+debug")).is_ok());
        // A comma separates the arguments of a make function, a blank the
        // entries of MAKEFILES, a colon a rule's targets from the rest, an
        // `=` makes a rule an assignment, and so on.
        for bad in ["/a,b", "/a:b", "/a=b", "/work/my files/c", "/a$b", "/a#b", "/a(b)", "/it's", "/a%b", "/a\\b", "/a|b", "/caf\u{e9}"] {
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
        let inputs = crate::objcache::tests::inputs(cactup.to_str().unwrap(), &config);
        crate::objcache::stage(&cc, crate::objcache::Mode::Record, &inputs).unwrap().unwrap();
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
        assert!(build.is_dir(), "the self-test runs there");
        let inject = inject_path(&cc);
        assert!(text.contains(&format!("\n{}: ;\n", inject.display())), "{text}");
        assert!(text.contains(&format!("ifneq ($(findstring |{}/,|$(CURDIR)/),)\n", build.display())), "{text}");
        assert!(text.contains(&format!("MAKEFILES := $(filter-out {},$(MAKEFILES))\n", inject.display())), "{text}");
        assert!(
            text.contains(&format!(
                "define cactup_cc_run\n'{}' __cc '{}' '$(subst ','\\'',$1)' '$(subst ','\\'',$(SHELL))'\nendef\n",
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
    fn a_new_probe_starts_the_attempts_cache_files_afresh() {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let cc = fake_build(&root, RULES);
        fs::create_dir_all(selftest_dir(&cc)).unwrap();
        fs::write(selftest_passed(&cc, "wrapped"), "").unwrap();
        fs::write(super::super::events_path(&cc), "{}\n").unwrap();
        probe(&conf_path(&cc)).unwrap();
        assert!(!selftest_passed(&cc, "wrapped").exists(), "an earlier self-test's pass is not this one's");
        assert!(!super::super::events_path(&cc).exists(), "an earlier run's compiles are not this build's");
    }

    #[test]
    fn a_build_directory_that_is_a_link_is_named_by_where_it_leads() {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let cc = fake_build(&root, RULES);
        let roomy = root.join("scratch-fs/sim-build");
        fs::create_dir_all(&roomy).unwrap();
        std::os::unix::fs::symlink(&roomy, root.join("Cactus/configs/sim/build")).unwrap();
        probe(&conf_path(&cc)).unwrap();
        let text = fs::read_to_string(inject_path(&cc)).unwrap();
        assert!(text.contains(&format!("ifneq ($(findstring |{}/,|$(CURDIR)/),)\n", roomy.display())), "{text}");
    }

    #[test]
    fn a_probe_that_declines_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let config = root.join("Cactus/configs/sim");

        // Rules that are not the ones it knows.
        let cc = fake_build(&root, "%.c.o: $(SRCDIR)/%.c\n\t$(CC) -c $<\n");
        let err = format!("{:#}", probe(&conf_path(&cc)).unwrap_err());
        assert!(err.contains("defines no compile recipe cactup knows how to wrap"), "{err}");

        // Rules spread over several files.
        fs::write(config.join("config-data/make.config.rules"), format!("{RULES}include $(CONFIG)/more.rules\n")).unwrap();
        let err = format!("{:#}", probe(&conf_path(&cc)).unwrap_err());
        assert!(err.contains("includes other makefiles"), "{err}");

        // Not configured yet.
        fs::remove_file(config.join("config-data/make.config.rules")).unwrap();
        let err = format!("{:#}", probe(&conf_path(&cc)).unwrap_err());
        assert!(err.contains("cannot read") && err.contains("make.config.rules"), "{err}");

        assert!(!inject_path(&cc).exists() && !selftest_dir(&cc).exists());
        assert!(!config.join("build").exists(), "declining must leave the configuration as it was");
    }
}
