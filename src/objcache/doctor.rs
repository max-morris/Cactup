//! `cactup cache doctor`'s checks of a configuration's build environment
//! (§18.12, M5 part 2): the cache's own rules, applied where its compiles
//! would run, without a build.
//!
//! The checks run in the build's own environment: `cactup cache doctor`
//! starts the attempt's frozen environment setup in a shell, and the shell
//! starts `cactup __cc-doctor` ([`DOCTOR_VERB`]) with what to check on
//! stdin. That process applies to each language's compiler and flags what
//! a compile through the wrapper would ([`run`]): the environment, the
//! driver's own searches, the compiler's identity, the flags (each variable
//! on its own, so that every cause is found and not the first only), and
//! a whole key of a small probe compile (the preprocessor and the
//! identity's trial compiles run; no object of the build is compiled), and
//! again where a thorn's compiles get other compilers or flags (read with
//! make through Cactus's own `make.subdir`, [`thorn_checks`]). It answers
//! in JSON on stdout; `cache doctor` says where each finding comes from (the
//! user's shell, the machine's environment setup, the option list,
//! Cactus's defaults, a thorn's file) and what to change ([`Finding`]).
//!
//! The findings are a small seam ([`CheckSet`]): named sets of checks
//! returning findings, so that a general `cactup doctor` can run them with
//! others.

use super::{compile, environment, identity, key, BuildConf, Mode};
use crate::Res;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsString;
#[cfg(test)]
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

/// The verb the build environment's shell runs the checks by: `cactup
/// __cc-doctor`, with a [`Request`] on stdin.
pub const DOCTOR_VERB: &str = "__cc-doctor";

/// How much a finding matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Severity {
    /// Not checked (the setup did not run to its end, the build runs in a
    /// container): what follows says nothing of it.
    Unchecked,
    /// The cache stays out of these compiles.
    KeepsOut,
    /// The cache takes part, but shares less (fewer objects between
    /// installations or configurations), or pays more for each compile.
    Costs,
    /// Worth knowing.
    Note,
}

/// One thing a check found: how much it matters, what it is, where it comes
/// from, and what to change.
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub severity: Severity,
    pub what: String,
    pub from: String,
    pub fix: Option<String>,
}

/// A named set of checks.
pub struct CheckSet<'a> {
    pub name: &'static str,
    pub run: Box<dyn Fn() -> Vec<Finding> + 'a>,
}

/// One language a configuration compiles, as its compile recipe names it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Language {
    /// "C", "C++", "Fortran", "CUDA".
    pub name: String,
    /// The make variable of the compiler (`CC`) and of its flags in the
    /// order the recipe gives them (`CPPFLAGS`, `CFLAGS`).
    pub compiler_variable: String,
    pub flag_variables: Vec<String>,
    /// The suffix of a source of the language.
    pub suffix: String,
    /// A source the compiler accepts.
    pub probe: String,
}

/// What `cache doctor` asks of the process in the build environment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// The configuration's directory, whose `config-data/make.config.defn`
    /// has the compilers and flags as configured (`None`: not configured).
    pub config_dir: Option<PathBuf>,
    /// The values the option list gives, where there is no configuration.
    pub optionlist: BTreeMap<String, String>,
    /// The cache's settings a build would freeze (the probe's key is made
    /// under them).
    pub conf: BuildConf,
    pub languages: Vec<Language>,
}

/// One thing found in the build environment, before `cache doctor` says
/// where it comes from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Raw {
    pub severity: Severity,
    /// What it is about: a language ("C++"), or none for every compile.
    pub language: Option<String>,
    /// The make variable it is in, or the environment variable it is about.
    pub variable: Option<String>,
    pub reason: String,
    /// The thorn (and the file of it) that adds the flag, for a flag only
    /// a thorn's own compiles get.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thorn: Option<String>,
    /// What to change, where the check knows it better than `cache
    /// doctor` would (a thorn's flag).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

/// What the process in the build environment answers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    /// The environment variables the cache reads for its rules, as the
    /// build environment has them.
    pub env: BTreeMap<String, String>,
    /// The compilers and flags as `make` gives them (or the option list).
    pub values: BTreeMap<String, String>,
    pub found: Vec<Raw>,
}

/// The languages Cactus's compile recipes (`COMPILE_C`, `COMPILE_CXX`,
/// `COMPILE_F90`, `COMPILE_CU`) compile, with the variables they read.
pub fn languages() -> Vec<Language> {
    let language = |name: &str, compiler: &str, flags: &[&str], suffix: &str, probe: &str| Language {
        name: name.to_owned(),
        compiler_variable: compiler.to_owned(),
        flag_variables: flags.iter().map(|flag| (*flag).to_owned()).collect(),
        suffix: suffix.to_owned(),
        probe: probe.to_owned(),
    };
    vec![
        language("C", "CC", &["CPPFLAGS", "CFLAGS"], "c", "#include <stdlib.h>\nint probe(void) { return EXIT_SUCCESS; }\n"),
        language("C++", "CXX", &["CPPFLAGS", "CXXFLAGS"], "cc", "#include <vector>\nint probe() { return std::vector<int>(1).size(); }\n"),
        language("Fortran", "F90", &["F90FLAGS"], "f90", "subroutine probe()\nend subroutine\n"),
        language("Fortran 77", "F77", &["F77FLAGS"], "f", "      subroutine probe\n      end\n"),
        language("CUDA", "CUCC", &["CUCCFLAGS"], "cu", "__global__ void probe() {}\n"),
    ]
}

/// The environment variables whose entries the cache's rules look at, and
/// that a shell's `export X=/dir:$X` can give an empty entry.
pub const PATH_VARIABLES: [&str; 9] =
    ["PATH", "LD_LIBRARY_PATH", "LIBRARY_PATH", "CPATH", "C_INCLUDE_PATH", "CPLUS_INCLUDE_PATH", "COMPILER_PATH", "GCC_EXEC_PREFIX", "LD_PRELOAD"];

/// Is `entry`, of the path variable `variable`, not an absolute path? A
/// library `LD_PRELOAD` names without a `/` is searched for, as the loader
/// does: only a relative path with one is relative there (and a blank
/// between its entries is no entry).
fn relative_entry(variable: &str, entry: &[u8]) -> bool {
    match variable {
        "LD_PRELOAD" => entry.contains(&b'/') && !entry.starts_with(b"/"),
        _ => !entry.starts_with(b"/"),
    }
}

/// `cactup __cc-doctor`: read a [`Request`] on stdin, run the checks in
/// this process's environment (the build's), and write the [`Reply`] on
/// stdout. Status 1 if the request cannot be read.
pub fn run_verb() -> i32 {
    let mut input = Vec::new();
    let Ok(request) = std::io::stdin().read_to_end(&mut input).map_err(|e| e.to_string()).and_then(|_| serde_json::from_slice::<Request>(&input).map_err(|e| e.to_string())) else {
        return 1;
    };
    // Its own process: the probe's compile may run in its own directory.
    let reply = run(&request, true);
    let _ = std::io::stdout().lock().write_all(&serde_json::to_vec(&reply).unwrap_or_default());
    0
}

/// The checks, in this process's environment; with `probe`, a whole key of
/// a small compile too, which runs in a directory of its own (this process
/// moves there and back: only for a process of its own).
pub fn run(request: &Request, probe: bool) -> Reply {
    let mut reply = Reply::default();
    for variable in PATH_VARIABLES.iter().chain(environment::REFUSED.iter()).chain(["LD_AUDIT"].iter()) {
        if let Some(value) = std::env::var_os(variable) {
            reply.env.insert((*variable).to_owned(), value.to_string_lossy().into_owned());
        }
    }
    reply.values = match &request.config_dir {
        Some(config_dir) => made_values(config_dir, &request.languages).unwrap_or_else(|e| {
            reply.found.push(Raw {
                severity: Severity::Note,
                language: None,
                variable: None,
                reason: format!("the compilers and flags are the option list's: make could not give the configuration's ({e:#})"),
                thorn: None,
                fix: None,
            });
            request.optionlist.clone()
        }),
        None => request.optionlist.clone(),
    };
    // The environment: what keeps every compile out.
    for reason in environment::refusals(std::env::vars_os()) {
        let variable = reason.split_whitespace().next().map(str::to_owned);
        reply.found.push(Raw { severity: Severity::KeepsOut, language: None, variable, reason, thorn: None, fix: None });
    }
    // Entries of the path variables that are not absolute: some keep a
    // kind of compile out (below, in the probe's key); every one makes each
    // compile look at more places.
    for variable in PATH_VARIABLES {
        if let Some(value) = std::env::var_os(variable)
            && value.as_encoded_bytes().split(|b| *b == b':' || (variable == "LD_PRELOAD" && *b == b' ')).any(|entry| relative_entry(variable, entry))
        {
            reply.found.push(Raw {
                severity: Severity::Note,
                language: None,
                variable: Some(variable.to_owned()),
                reason: format!("{variable} has an entry that is not an absolute path (an empty one is the working directory)"),
                thorn: None,
                fix: None,
            });
        }
    }
    let Ok(tmp) = tempfile::tempdir() else { return reply };
    for language in &request.languages {
        reply.found.extend(check_language(request, language, &reply.values, tmp.path(), probe));
    }
    // What thorns give their own compiles beyond the configuration's.
    if let Some(config_dir) = &request.config_dir {
        let thorns = thorn_checks(request, config_dir, &reply.values, &reply.found, tmp.path(), probe);
        reply.found.extend(thorns);
    }
    reply
}

/// Do the flags `flags` pass the wrapper's reader of a compile of a source
/// named `source` with a compiler of `family`?
fn parse_flags(family: identity::Family, flags: &[OsString], source: &str) -> Result<(), String> {
    let mut args: Vec<OsString> = flags.to_vec();
    args.extend(["-c", "-o", "probe.o", source].map(OsString::from));
    match family {
        identity::Family::Nvcc => compile::parse_cuda(&args).map(|_| ()),
        _ => compile::parse(&args).map(|_| ()),
    }
}

/// What each thorn of the configuration at `config_dir` gives its own
/// compiles beyond what the configuration gives every compile: for each
/// directory a thorn compiles (its `src`, and each of its `SUBDIRS`), the
/// compiler and flags of each of its objects as Cactus's own `make.subdir`
/// gives its compile there ([`made_compiles`]). Where an object's compiler
/// or flags differ from the configuration's, its language is checked again
/// as for the configuration (`check_language`, the probe's key too), and
/// what that finds that the configuration's check did not is the thorn's,
/// for the objects that get it, traced to the file that has the flag. A
/// directory `make` cannot read is said so.
fn thorn_checks(request: &Request, config_dir: &Path, values: &BTreeMap<String, String>, base: &[Raw], tmp: &Path, probe: bool) -> Vec<Raw> {
    let cactus_root = &request.conf.cactus_root;
    let thorns: Vec<String> = std::fs::read_to_string(config_dir.join("ThornList"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.split('#').next())
        .filter_map(|line| line.split_whitespace().next())
        .filter(|thorn| thorn.contains('/'))
        .map(str::to_owned)
        .collect();
    // Each thorn's compiled directories: `src`, and its `SUBDIRS` (as make
    // reads them, links and all).
    let tops: Vec<(String, PathBuf)> = thorns.iter().map(|thorn| (thorn.clone(), cactus_root.join("arrangements").join(thorn).join("src"))).filter(|(_, src)| src.is_dir()).collect();
    let tops = crate::par::parallel_map(&tops, |(thorn, src)| (thorn.clone(), src.clone(), made_compiles(request, config_dir, thorn, src)));
    let Ok(tops) = tops else { return Vec::new() };
    let mut dirs: Vec<ThornDir> = Vec::new();
    let mut subdirs = Vec::new();
    for (thorn, src, made) in tops {
        if let Ok(made) = &made {
            subdirs.extend(made.subdirs.iter().map(|sub| (thorn.clone(), src.join(sub))));
        }
        dirs.push((thorn, src, made));
    }
    let subdirs = crate::par::parallel_map(&subdirs, |(thorn, dir)| (thorn.clone(), dir.clone(), made_compiles(request, config_dir, thorn, dir)));
    let Ok(subdirs) = subdirs else { return Vec::new() };
    dirs.extend(subdirs);

    let mut found = Vec::new();
    let mut memo = Memo::new();
    for (thorn, dir, made) in dirs {
        if gix::interrupt::is_triggered() {
            break;
        }
        let shown = dir.strip_prefix(cactus_root).unwrap_or(&dir).display().to_string();
        let unchecked = |reason: String| Raw { severity: Severity::Unchecked, language: None, variable: None, reason, thorn: Some(format!("{thorn} ({shown})")), fix: None };
        let made = match made {
            Ok(made) => made,
            Err(e) => {
                found.push(unchecked(format!("make could not say what the thorn's compiles there get: {e:#}")));
                continue;
            }
        };
        if made.compiles.is_empty() && made.failed.is_none() {
            continue;
        }
        let name = thorn.rsplit('/').next().unwrap_or(&thorn);
        // A directory whose make files might define compile recipes of
        // their own: the fragment stands down there, and nothing is cached.
        if let Some(line) = stands_down(&dir) {
            let file = line.0.strip_prefix(cactus_root).unwrap_or(&line.0).display().to_string();
            found.push(Raw {
                severity: Severity::KeepsOut,
                language: None,
                variable: None,
                reason: format!("line {} of {file} (\"{}\") might define compile recipes of the directory's own, so the build cache stands down for its compiles", line.1, line.2),
                thorn: Some(format!("{thorn} ({shown})")),
                fix: Some(format!(
                    "the cache stays out of this directory's compiles only. It stands down wherever a thorn's make.code.defn or make.code.deps has an include, a define, an $(eval) or a COMPILE_ name, a comment too: the thorn's to change, if {file} can do without it"
                )),
            });
            continue;
        }
        if let Some(why) = &made.failed {
            found.push(unchecked(format!("make could not say what {} of the thorn's compiles there get: {why}", made.missing)));
        }
        // What each object's compile finds, one finding with the objects
        // that get it.
        let mut theirs: Vec<(Raw, Vec<&str>)> = Vec::new();
        for (object, language, own) in &made.compiles {
            for raw in differences(request, language, values, own, base, tmp, probe, &mut memo) {
                let file = flag_file(&raw, own, &dir, config_dir, name);
                let place = file.as_ref().map(|file| file.strip_prefix(cactus_root).map(Path::to_path_buf).unwrap_or(file.clone()).display().to_string());
                let fix = Some(thorn_fix(&raw, file.as_deref(), config_dir));
                let raw = Raw { thorn: Some(place.unwrap_or_else(|| shown.clone())), fix, ..raw };
                match theirs.iter_mut().find(|(seen, _)| *seen == raw) {
                    Some((_, objects)) => objects.push(object),
                    None => theirs.push((raw, vec![object])),
                }
            }
        }
        for (raw, objects) in theirs {
            // All the directory's compiles of the language, or those named.
            let all = made.compiles.iter().filter(|(_, language, _)| Some(&language.name) == raw.language.as_ref()).count();
            let place = raw.thorn.clone().unwrap_or_default();
            let thorn = match objects.len() {
                n if n == all => format!("{thorn} ({place})"),
                n if n <= 3 => format!("{thorn} ({place}, for {})", objects.join(", ")),
                n => format!("{thorn} ({place}, for {} and {} more)", objects[..2].join(", "), n - 2),
            };
            found.push(Raw { thorn: Some(thorn), ..raw });
        }
    }
    found
}

/// A thorn, one of its directories, and what make gives its compiles there.
type ThornDir = (String, PathBuf, Res<Made>);

/// The language checks made again, by language and the values checked.
type Memo = std::collections::HashMap<(String, Vec<(String, String)>), Vec<Raw>>;

/// What the language `language` finds with an object's values `own` that
/// it did not with `values` and the configuration's check (`base`): where
/// its compiler or flags differ, it is checked again, the probe's key too,
/// once for each set of its values (`memo`); each finding gets the variable
/// it is about where the probe's reason names a flag of its.
#[allow(clippy::too_many_arguments)]
fn differences(
    request: &Request,
    language: &Language,
    values: &BTreeMap<String, String>,
    own: &BTreeMap<String, String>,
    base: &[Raw],
    tmp: &Path,
    probe: bool,
    memo: &mut Memo,
) -> Vec<Raw> {
    let variables: Vec<&String> = std::iter::once(&language.compiler_variable).chain(&language.flag_variables).collect();
    let words = |values: &BTreeMap<String, String>, variable: &str| {
        let value = values.get(variable).cloned().unwrap_or_default();
        super::cuda::shell_words(&value, |name| std::env::var(name).ok()).unwrap_or_else(|_| value.split_whitespace().map(str::to_owned).collect())
    };
    if variables.iter().all(|variable| words(values, variable) == words(own, variable)) {
        return Vec::new();
    }
    let mut merged = values.clone();
    for variable in &variables {
        merged.insert((*variable).clone(), own.get(*variable).cloned().unwrap_or_default());
    }
    let key = (language.name.clone(), variables.iter().map(|variable| ((*variable).clone(), merged.get(*variable).cloned().unwrap_or_default())).collect());
    let again = memo.entry(key).or_insert_with(|| check_language(request, language, &merged, tmp, probe)).clone();
    let already = |raw: &Raw| base.iter().any(|seen| seen.reason == raw.reason && seen.language == raw.language);
    again.into_iter().filter(|raw| !already(raw)).map(|raw| with_flag_variable(&raw, &merged)).collect()
}

/// What `make` gives the compiles of one directory of a thorn's.
#[derive(Debug, Default)]
struct Made {
    /// Its `SUBDIRS`.
    subdirs: Vec<String>,
    /// Each object Cactus's compile recipes build, with its language and
    /// its compiler and flags.
    compiles: Vec<(String, Language, BTreeMap<String, String>)>,
    /// Why make could not say what some objects get, and how many.
    failed: Option<String>,
    missing: usize,
}

/// What Cactus's own `make.subdir` gives each compile of thorn `thorn` in
/// its directory `dir`, as the build's sub-make there would: the
/// configuration's `make.config.defn`, the thorn's bindings and the
/// capabilities they include, the directory's `make.code.defn` and
/// `make.code.deps`, `make.configuration.defn`, its goal (an object gets
/// what is set for the goal it is made for), each object made by the
/// pattern rule it is made by (`$*` is its stem), target- and
/// pattern-specific values all included.
///
/// Asked with `make -n` for the goal, with the compile recipes (`COMPILE_C`
/// and the rest, as the fragment redefines them) redefined to say each
/// object's values instead. Nothing is built: `make -n` runs no recipe but
/// the lines it runs all the same (a `+` line, one with `$(MAKE)`, the
/// remaking of a file it includes), and those go to a shell that runs none
/// of them. That shell runs what make's `$(shell)` asks, as the build would:
/// make reads its output through a pipe, where a recipe's goes to make's
/// own, a file here. In a directory of its own, where no object or
/// dependency file of the build is.
fn made_compiles(request: &Request, config_dir: &Path, thorn: &str, dir: &Path) -> Res<Made> {
    let cactus_root = &request.conf.cactus_root;
    let name = thorn.rsplit('/').next().unwrap_or(thorn);
    let tmp = tempfile::tempdir().context("Failed to make a temporary directory")?;
    // The recipe lines' shell, read as a script by /bin/sh: no line of a
    // recipe's runs; `$(shell)`'s does.
    let shell = tmp.path().join("recipe-shell");
    std::fs::write(&shell, "if [ -p /dev/stdout ]; then shift; exec /bin/sh -c \"$@\"; fi\n").context("Failed to write a script")?;
    if shell.to_string_lossy().contains(char::is_whitespace) {
        anyhow::bail!("the temporary directory's name has a blank in it");
    }
    const OBJECTS: &str = "cactup-doctor-objects ";
    const SUBDIRS: &str = "cactup-doctor-subdirs ";
    const COMPILE: &str = "cactup-doctor-compile ";
    const VALUE: &str = "cactup-doctor-value ";
    let mut print = format!("$(info {OBJECTS}$(OBJS))\n$(info {SUBDIRS}$(SUBDIRS))\n");
    // Cactus's rules for the dependency files make.subdir includes,
    // cancelled: none is made (not even by the shell that runs nothing,
    // one for each source).
    for suffix in ["c", "cc", "C", "cpp", "cxx", "cl", "cu", "F77", "f77", "F", "f", "F90", "f90"] {
        print.push_str(&format!("%.{suffix}.d: $(SRCDIR)/%.{suffix}\n"));
    }
    for (recipe, compiler) in super::probe::RECIPES {
        let Some(language) = request.languages.iter().find(|language| language.compiler_variable == *compiler) else { continue };
        print.push_str(&format!("override define {recipe}\n{COMPILE}{compiler} $@\n"));
        for variable in std::iter::once(&language.compiler_variable).chain(&language.flag_variables) {
            print.push_str(&format!("{VALUE}{variable}=$({variable})\n"));
        }
        print.push_str("endef\n");
    }
    std::fs::write(tmp.path().join("cactup-doctor.mk"), print).context("Failed to write a makefile")?;
    let out = run_to_files(
        Command::new("make")
            .args(["-n", "-k", "--no-print-directory", "-f"])
            .arg(cactus_root.join("lib/make/make.subdir"))
            .args(["-f", "cactup-doctor.mk", "make.checked", "CCTK_TARGET=make.checked"])
            .arg(format!("TOP={}", config_dir.display()))
            .arg(format!("CONFIG={}/config-data", config_dir.display()))
            .arg(format!("SRCDIR={}", dir.display()))
            .arg(format!("THORN={name}"))
            .arg("SHELL=/bin/sh")
            .arg(format!(".SHELLFLAGS={} -c", shell.display()))
            // What the build's own makes export to it.
            .env("CCTK_HOME", cactus_root)
            .env("MAKE_DIR", cactus_root.join("lib/make"))
            .env("SCRATCH_BUILD", config_dir.join("scratch"))
            .env_remove("MAKEFILES")
            .env_remove("MAKEFLAGS")
            .env_remove("MAKELEVEL")
            .current_dir(tmp.path()),
        None,
    )?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    // (Said again by a make that started over after remaking a file it
    // includes: the last is what counts.)
    let said = |mark: &str| stdout.lines().filter_map(|line| line.strip_prefix(mark.trim_end())).next_back().map(|rest| rest.split_whitespace().map(str::to_owned).collect::<Vec<_>>());
    let last = |mark: &str| said(mark).unwrap_or_default();
    let objects = said(OBJECTS);
    let mut made = Made { subdirs: last(SUBDIRS), ..Made::default() };
    for line in stdout.lines().map(str::trim) {
        if let Some((compiler, object)) = line.strip_prefix(COMPILE).and_then(|rest| rest.split_once(' ')) {
            let Some(language) = request.languages.iter().find(|language| language.compiler_variable == compiler) else { continue };
            made.compiles.push((object.to_owned(), language.clone(), BTreeMap::new()));
        } else if let (Some((_, _, values)), Some((variable, value))) = (made.compiles.last_mut(), line.strip_prefix(VALUE).and_then(|rest| rest.split_once('='))) {
            values.insert(variable.to_owned(), value.trim().to_owned());
        }
    }
    if !out.status.success() {
        let said = String::from_utf8_lossy(&out.stderr);
        let why = said.lines().rev().find(|line| !line.trim().is_empty()).unwrap_or("make failed").to_owned();
        // Its objects not said: make did not read its files through.
        let Some(objects) = objects else { anyhow::bail!("{why}") };
        made.missing = objects.iter().filter(|object| !made.compiles.iter().any(|(made, _, _)| made == *object)).count();
        if made.missing > 0 {
            made.failed = Some(why);
        }
    }
    Ok(made)
}

/// The first line of `dir`'s `make.code.defn` or `make.code.deps` the
/// fragment stands down for ([`super::probe::STAND_DOWN`], as the probe
/// asks it of `grep -E`): the file, its line number and the line.
fn stands_down(dir: &Path) -> Option<(PathBuf, usize, String)> {
    [dir.join("make.code.defn"), dir.join("make.code.deps")].into_iter().filter(|file| file.is_file()).find_map(|file| {
        let out = Command::new("grep").args(["-E", "-n", "-m", "1", "-e", super::probe::STAND_DOWN]).arg(&file).stdin(Stdio::null()).output().ok()?;
        let said = String::from_utf8_lossy(&out.stdout);
        let (number, line) = said.lines().next()?.split_once(':')?;
        Some((file.clone(), number.parse().ok()?, line.trim().to_owned()))
    })
}

/// What to change for a flag a thorn gives its compiles, set in `file`
/// where one was found: the thorn's file, or what generates it.
fn thorn_fix(raw: &Raw, file: Option<&Path>, config_dir: &Path) -> String {
    let flag = raw.reason.split([' ', ',']).next().filter(|word| word.starts_with('-'));
    let leave = flag.map(|flag| format!(" (leave {flag} out, if its compiles do not need it)")).unwrap_or_default();
    let generated = file.is_some_and(|file| file.starts_with(config_dir.join("bindings")));
    let capability = file.is_some_and(|file| file.starts_with(config_dir.join("bindings/Configuration/Capabilities")));
    let r#where = match (file, capability, generated) {
        (None, _, _) => "its make files make it (make.code.defn, make.code.deps, or what they include); it is not the option list's to change".to_owned(),
        (Some(_), true, _) => "it comes with a capability the thorn requires: the file named is written by the thorn that provides the capability (an ExternalLibraries thorn takes it from its configuration, or from the option list's variables for that library), so change it there".to_owned(),
        (Some(_), false, true) => "the file named is written from the thorn's configuration.ccl and what it requires: change it there".to_owned(),
        (Some(_), false, false) => "the file named sets it: the thorn's to change, not the option list's".to_owned(),
    };
    format!("the cache stays out of these compiles only; {where}{leave}")
}

/// The file of a thorn's that has the flag `raw` is about: the directory's
/// `make.code.defn` or `make.code.deps`, the thorn's bindings, or a
/// capability those include; none where no file names it.
fn flag_file(raw: &Raw, theirs: &BTreeMap<String, String>, dir: &Path, config_dir: &Path, name: &str) -> Option<PathBuf> {
    let variable = raw.variable.as_ref()?;
    let value = theirs.get(variable)?;
    let flag = value.split_whitespace().find(|flag| raw.reason.split([' ', ',', '(', ')']).any(|word| word == *flag) || (flag.ends_with("native") && raw.reason.contains("native")))?;
    let bindings = config_dir.join("bindings/Configuration");
    let thorn_defn = bindings.join(format!("Thorns/make.{name}.defn"));
    let mut candidates = vec![dir.join("make.code.defn"), dir.join("make.code.deps"), thorn_defn.clone(), bindings.join(name).join("make.configuration.defn")];
    // The capabilities the thorn's bindings include.
    if let Ok(text) = std::fs::read_to_string(&thorn_defn) {
        candidates.extend(text.split_whitespace().filter(|word| word.contains("Capabilities/make.")).filter_map(|word| PathBuf::from(word.trim_matches(['"', '\''])).file_name().map(|file| bindings.join("Capabilities").join(file))));
    }
    // In a line that is no comment.
    candidates.into_iter().find(|file| std::fs::read_to_string(file).is_ok_and(|text| text.lines().any(|line| !line.trim_start().starts_with('#') && line.split('#').next().unwrap_or_default().contains(flag))))
}

/// The compiler and flags of `language`, as `values` has them, checked as a
/// compile of it through the wrapper would be.
fn check_language(request: &Request, language: &Language, values: &BTreeMap<String, String>, tmp: &Path, probe: bool) -> Vec<Raw> {
    let mut found = Vec::new();
    let raw = |severity, variable: Option<&str>, reason: String| Raw { severity, language: Some(language.name.clone()), variable: variable.map(str::to_owned), reason, thorn: None, fix: None };
    let Some(compiler) = values.get(&language.compiler_variable).filter(|value| !value.trim().is_empty() && value.trim() != "none") else {
        return found;
    };
    // What the wrapper makes of the compiler's text before any key: a
    // command only a shell can run, another wrapper in front, a name the
    // recipe's shell gives a meaning of its own.
    let Some(words) = super::wrapper::plain_words(compiler.as_bytes()) else {
        found.push(raw(Severity::KeepsOut, Some(&language.compiler_variable), format!("{compiler} is not a plain command naming a program, so the recipe's shell runs it")));
        return found;
    };
    let name = words[0].clone();
    if Path::new(&name).file_name().and_then(|name| name.to_str()).is_some_and(|program| super::wrapper::OTHER_WRAPPERS.contains(&program)) {
        found.push(raw(Severity::KeepsOut, Some(&language.compiler_variable), format!("{} already stands in front of the compiler, and the cache does not stack on another wrapper", name.to_string_lossy())));
        return found;
    }
    // Not there at all: this configuration may not compile the language.
    if identity::find_program(&name).is_err() {
        found.push(raw(Severity::Note, Some(&language.compiler_variable), format!("{} is not on PATH in the build environment", name.to_string_lossy())));
        return found;
    }
    let cc_dir = tmp.join(format!("cc-{}", language.suffix));
    let _ = std::fs::create_dir_all(&cc_dir);
    // Make runs Cactus's recipes with bash (Cactus sets SHELL).
    let shell = ["/bin/bash", "/bin/sh"].into_iter().map(Path::new).find(|shell| shell.is_file()).unwrap_or(Path::new("/bin/sh"));
    if let Err(why) = super::lookup::shell_runs_program(&cc_dir, shell, &name) {
        found.push(raw(Severity::KeepsOut, Some(&language.compiler_variable), format!("{why}, so the recipe's shell runs it")));
        return found;
    }
    let fortran = language.name.starts_with("Fortran");
    if let Some(why) = key::driver_led_elsewhere(&name, fortran, |variable| std::env::var_os(variable)) {
        found.push(raw(Severity::KeepsOut, None, why));
    }
    let compiler = match identity::identify_watching(&cc_dir, &name) {
        Ok((compiler, _)) => compiler,
        Err(e) => {
            found.push(raw(Severity::KeepsOut, Some(&language.compiler_variable), format!("{e:#}")));
            return found;
        }
    };
    if !compiler.relocates {
        found.push(raw(
            Severity::Costs,
            Some(&language.compiler_variable),
            "the compiler did not make one object of the trial in two places, so its objects are not shared between installations or configurations".to_owned(),
        ));
    }
    // Each variable of flags on its own, so that every flag the cache does
    // not follow is found, with the variable it is in.
    let source = format!("probe.{}", language.suffix);
    let parse = |flags: &[OsString]| parse_flags(compiler.family, flags, &source);
    // The compiler's own words after its name (`g++ -std=c++17`): the
    // compiler variable's, checked once.
    if let Err(why) = parse(&words[1..]) {
        found.push(raw(Severity::KeepsOut, Some(&language.compiler_variable), why));
    }
    let mut every_flag = Vec::new();
    for variable in &language.flag_variables {
        // As the recipe's shell splits them (`-DNAME="a b"` is one word).
        let value = values.get(variable).cloned().unwrap_or_default();
        let flags: Vec<OsString> = match super::cuda::shell_words(&value, |name| std::env::var(name).ok()) {
            Ok(words) => words.into_iter().map(OsString::from).collect(),
            Err(_) => value.split_whitespace().map(OsString::from).collect(),
        };
        if let Err(why) = parse(&flags) {
            found.push(raw(Severity::KeepsOut, Some(variable), why));
        }
        every_flag.extend(flags);
    }
    if !probe {
        return found;
    }
    // A whole key of a probe compile with every flag, as the wrapper makes
    // it: what only the combination, the environment or the compiler's
    // toolchain says.
    let config_dir = tmp.join("Cactus/configs/doctor");
    let dir = config_dir.join("build/Doctor");
    let scratch = config_dir.join("scratch");
    for created in [&dir, &scratch] {
        let _ = std::fs::create_dir_all(created);
    }
    let probe = dir.join(&source);
    if std::fs::write(&probe, &language.probe).is_err() {
        return found;
    }
    let conf = BuildConf { mode: Mode::Record, config_dir: config_dir.clone(), cactus_root: tmp.join("Cactus"), ..request.conf.clone() };
    let mut argv = words.clone();
    argv.extend(every_flag);
    argv.extend([OsString::from("-c"), "-o".into(), dir.join("probe.o").into(), probe.into()]);
    let here = std::env::current_dir();
    let _ = std::env::set_current_dir(&scratch);
    let keyed = key::key(&conf, &cc_dir, &argv, false);
    if let Ok(here) = here {
        let _ = std::env::set_current_dir(here);
    }
    match keyed {
        Ok(keyed) => {
            if conf.relocate && compiler.relocates && !keyed.relocatable() {
                found.push(raw(Severity::Costs, None, "its keys name this installation's paths, so its objects are not shared between installations or configurations".to_owned()));
            }
            if let Some(why) = keyed.unnarrowed.filter(|_| !fortran) {
                found.push(raw(Severity::Costs, None, format!("no file is narrowed ({why}): a header that differs between configurations keeps their objects apart")));
            }
        }
        // Said already by a check above, or new: the probe's own.
        Err(why) if !found.iter().any(|raw| raw.reason == why) => found.push(raw(Severity::KeepsOut, None, why)),
        Err(_) => {}
    }
    found
}

/// The compilers and flags as `make` gives them for the configuration at
/// `config_dir` (Cactus's `make.config.defn`, with its conditionals: the
/// optimizing flags where optimizing, and the like), in this environment.
fn made_values(config_dir: &Path, languages: &[Language]) -> Res<BTreeMap<String, String>> {
    let mut variables: Vec<String> = Vec::new();
    for language in languages {
        variables.push(language.compiler_variable.clone());
        variables.extend(language.flag_variables.iter().cloned());
    }
    variables.sort();
    variables.dedup();
    evaluate(config_dir, &variables)
}

/// The values of `variables` as `make` has them for the configuration at
/// `config_dir`.
fn evaluate(config_dir: &Path, variables: &[String]) -> Res<BTreeMap<String, String>> {
    let defn = config_dir.join("config-data/make.config.defn");
    if !defn.is_file() {
        anyhow::bail!("{} is not there", defn.display());
    }
    let dir = tempfile::tempdir().context("Failed to make a temporary directory")?;
    // `$(info)`: no shell between make and the values (a quote in one is
    // its own).
    let mut makefile = format!("TOP := {}\nCONFIG := {}/config-data\ninclude {}\n", config_dir.display(), config_dir.display(), defn.display());
    for variable in variables {
        makefile.push_str(&format!("$(info {variable}=$({variable}))\n"));
    }
    makefile.push_str("print: ; @:\n");
    std::fs::write(dir.path().join("print.mk"), makefile).context("Failed to write a makefile")?;
    let out = Command::new("make")
        .args(["-s", "-f", "print.mk", "print"])
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .context("Failed to run make")?;
    if !out.status.success() {
        anyhow::bail!("make could not read {}", defn.display());
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.to_owned(), value.trim().to_owned()))
        .collect())
}

/// The path variables an environment setup `env` gives an entry that is not
/// an absolute path, run with each of them unset, empty and set: `export
/// X=/dir:$X` gives `X` an empty entry (the working directory) where `X`
/// was not set before. Commands of module systems (`module`, `ml`) do
/// nothing here, and a file it sources that is not there does not stop it
/// (it runs in bash, which goes on). Each finding: the variable, how it was
/// before, its value after. An error if the setup does not run to its end
/// (an `exit`, `set -e` and a command that fails): what it would set after
/// is not known.
pub fn fragile_entries(env: &str) -> Result<Vec<(String, &'static str, String)>, String> {
    const END: &str = "cactup: the end of the setup";
    let mut found = Vec::new();
    for state in ["unset", "empty", "set"] {
        // What the setup prints goes to stderr; stdout is the values.
        let mut script = format!("module() {{ :; }}; ml() {{ :; }}\n{{\n:\n{env}\n}} >&2 </dev/null\nprintf '%s\\n' '{END}'\n");
        for variable in PATH_VARIABLES {
            script.push_str(&format!("printf '%s=%s\\n' {variable} \"${{{variable}-<unset>}}\"\n"));
        }
        let shell = ["/bin/bash", "/bin/sh"].into_iter().find(|shell| Path::new(shell).is_file()).unwrap_or("/bin/sh");
        let mut command = Command::new(shell);
        command.arg("-c").arg(&script).env_clear().env("HOME", "/nonexistent-home");
        for variable in PATH_VARIABLES {
            match state {
                "set" => {
                    command.env(variable, if variable == "PATH" { "/usr/bin:/bin" } else { "/opt/x" });
                }
                "empty" if variable != "PATH" => {
                    command.env(variable, "");
                }
                _ => {}
            }
        }
        if state != "set" {
            command.env("PATH", "/usr/bin:/bin");
        }
        let out = run_to_files(&mut command, None).map_err(|e| format!("{e:#}"))?;
        let out = String::from_utf8_lossy(&out.stdout).into_owned();
        let Some((_, values)) = out.split_once(&format!("{END}\n")) else {
            return Err(format!("with the path variables {state} before it, the setup did not run to its end (an exit, or a command that failed under set -e), so what it sets is not known"));
        };
        for line in values.lines() {
            let Some((variable, value)) = line.split_once('=').filter(|(variable, _)| PATH_VARIABLES.contains(variable)) else { continue };
            // Left as it came (unset, or empty as it was): not the setup's.
            if value == "<unset>" || (value.is_empty() && state != "set") {
                continue;
            }
            let entries: Vec<&str> = match variable {
                "LD_PRELOAD" => value.split([':', ' ']).collect(),
                _ => value.split(':').collect(),
            };
            if entries.iter().any(|entry| relative_entry(variable, entry.as_bytes())) && !found.iter().any(|(found, _, _): &(String, _, _)| found == variable) {
                found.push((variable.to_owned(), state, value.to_owned()));
            }
        }
    }
    Ok(found)
}

/// What the checks' shell says on stderr once the setup is through.
const SETUP_DONE: &str = "cactup: the environment setup ran to its end";

/// A script that runs the environment setup `env` as the build script does
/// (`set -e`), with its stdin `/dev/null` and its stdout sent to stderr,
/// and then `then`. (`:` first: a group with nothing in it, as an empty
/// setup or one of comments makes, is a syntax error.)
fn setup_then(env: &str, then: &str) -> String {
    format!("set -e\n{{\n:\n{env}\n}} >&2 </dev/null\n{then}\n")
}

/// Run the checks of [`Request`] in the build environment `env` (the
/// attempt's frozen setup), from `cactus_root`, by `cactup`.
pub fn in_build_environment(cactup: &Path, cactus_root: &Path, env: &str, request: &Request) -> Res<Reply> {
    // As the build script runs it (`set -e`), but what the setup prints goes
    // to stderr: stdout is the checks' answer.
    // A line on stderr once the setup is through: one that ends the shell
    // with status 0 (an `exit`, a `return`) is told from the checks.
    let script = setup_then(env, &format!("printf '%s\\n' '{SETUP_DONE}' >&2\nexec {} {DOCTOR_VERB}", crate::build::sh_quote(cactup)));
    let input = serde_json::to_vec(request).context("Failed to write the checks' request")?;
    let out = run_to_files(Command::new("/bin/sh").arg("-c").arg(&script).current_dir(cactus_root), Some(input))?;
    // An answer is the proof the setup ran through (its stderr may be gone:
    // `exec 2>/dev/null`).
    if out.status.success()
        && let Ok(reply) = serde_json::from_slice::<Reply>(&out.stdout)
    {
        return Ok(reply);
    }
    let said = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() && !said.contains(SETUP_DONE) {
        let last = said.lines().rev().find(|line| !line.trim().is_empty()).unwrap_or("it said nothing");
        anyhow::bail!("the build environment setup did not run to its end ({}): {last}", out.status);
    }
    if !said.contains(SETUP_DONE) {
        anyhow::bail!("the build environment setup ended the shell before its end (an exit, or a return), as the build's would");
    }
    serde_json::from_slice(&out.stdout).context("The checks in the build environment answered in a form this cactup cannot read")
}

/// Wait for `child`, stopping it on Ctrl-C (`CLAUDE.md`'s interrupt
/// contract): a module system's setup can take a while.
fn wait_interruptibly(mut child: std::process::Child) -> Res<std::process::ExitStatus> {
    // From 1 ms, up to 50: most of what runs here (a make in each of
    // hundreds of directories) is over in a few.
    let mut pause = std::time::Duration::from_millis(1);
    loop {
        if let Some(status) = child.try_wait().context("Failed to wait for the build environment")? {
            return Ok(status);
        }
        if gix::interrupt::is_triggered() {
            // The setup's process group: what it started too (it runs in
            // a group of its own).
            if let Some(group) = rustix::process::Pid::from_raw(child.id() as i32) {
                let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("interrupted");
        }
        std::thread::sleep(pause);
        pause = (pause * 2).min(std::time::Duration::from_millis(50));
    }
}

/// Run `command` with its stdout and stderr in files of their own, waiting
/// for it alone (a process it leaves in the background may keep them open:
/// what was written by the time it ends is all there is), with `input` on
/// its stdin. Ctrl-C stops it.
fn run_to_files(command: &mut Command, input: Option<Vec<u8>>) -> Res<std::process::Output> {
    let stdout = tempfile::tempfile().context("Failed to make a temporary file")?;
    let stderr = tempfile::tempfile().context("Failed to make a temporary file")?;
    let mut child = command
        .process_group(0)
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(stdout.try_clone().context("Failed to share a temporary file")?)
        .stderr(stderr.try_clone().context("Failed to share a temporary file")?)
        .spawn()
        .context("Failed to start the build environment's shell")?;
    // From a thread of its own: a setup that stops at once never reads it
    // (what it said is the finding, not a broken pipe), one that runs long
    // reads it late.
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
    }
    let status = wait_interruptibly(child)?;
    let read = |mut file: std::fs::File| {
        use std::io::Seek;
        let mut bytes = Vec::new();
        let _ = file.seek(std::io::SeekFrom::Start(0));
        let _ = file.read_to_end(&mut bytes);
        bytes
    };
    Ok(std::process::Output { status, stdout: read(stdout), stderr: read(stderr) })
}

/// The checks of a configuration's build environment, as the build
/// attempt `attempt` froze it (its setup and option list), with the cache's
/// settings `conf` would have: each finding with where it comes from and
/// what to change.
pub fn check_attempt(attempt: &crate::build::attempt::BuildAttempt, conf: BuildConf) -> Res<Vec<Finding>> {
    let meta = &attempt.meta;
    let mut findings = Vec::new();
    if let Some(universe) = meta.universe.as_ref().filter(|universe| universe.wrapper_argv.is_some() || universe.wrapper.is_some()) {
        findings.push(Finding {
            severity: Severity::Unchecked,
            what: format!("this configuration builds in the universe \"{}\", whose environment is not checked here", universe.name),
            from: "the configuration".to_owned(),
            fix: None,
        });
        return Ok(findings);
    }
    let optionlist = optionlist_values(&attempt.optionlist_path());
    let defaults = configured_values(&meta.config_dir);
    let configured = meta.config_dir.join("config-data/make.config.defn").is_file();
    if !configured {
        findings.push(Finding {
            severity: Severity::Note,
            what: "the configuration is not configured yet: the compilers and flags checked are the option list's, without Cactus's defaults".to_owned(),
            from: "the configuration".to_owned(),
            fix: None,
        });
    }
    let request = Request {
        config_dir: configured.then(|| meta.config_dir.clone()),
        optionlist: optionlist.clone(),
        conf,
        languages: languages(),
    };
    let cactup = std::env::current_exe().context("Failed to find this cactup")?;
    let reply = in_build_environment(&cactup, &meta.cactus_root, &meta.build_env, &request)?;
    let source = format!("the option list ({})", meta.optionlist_source);
    let setup = format!("the build environment setup of machine {} (`cactup machine show {}`)", meta.machine, meta.machine);
    // One finding per reason and variable: the languages it was found for
    // (none, when it is about every compile), and the most it matters. A
    // probe's reason with no variable is about a flag of the language's,
    // where one is in it.
    let found: Vec<Raw> = reply.found.iter().map(|raw| with_flag_variable(raw, &reply.values)).collect();
    let mut merged: Vec<(Severity, &Raw, Vec<&str>)> = Vec::new();
    for raw in &found {
        match merged.iter_mut().find(|(_, seen, _)| seen.reason == raw.reason && seen.thorn == raw.thorn && (seen.variable == raw.variable || seen.variable.is_none() || raw.variable.is_none())) {
            Some((severity, seen, languages)) => {
                *severity = (*severity).min(raw.severity);
                // The one that names where it is.
                if seen.variable.is_none() && raw.variable.is_some() {
                    *seen = raw;
                }
                match &raw.language {
                    Some(language) if !languages.is_empty() && !languages.contains(&language.as_str()) => languages.push(language),
                    Some(_) if !languages.is_empty() => {}
                    Some(_) => {}
                    None => languages.clear(),
                }
            }
            None => merged.push((raw.severity, raw, raw.language.iter().map(String::as_str).collect())),
        }
    }
    let every = languages().len();
    for (severity, raw, languages) in merged {
        let what = match languages.len() {
            0 => raw.reason.clone(),
            n if n == every => raw.reason.clone(),
            _ => format!("{}: {}", languages.join(", "), raw.reason),
        };
        let (from, fix) = trace(raw, &reply, &optionlist, &defaults, &source, &setup);
        findings.push(Finding { severity, what, from, fix });
    }
    // A setup that gives a path variable an empty entry where it was not
    // set before: fine for this user now, not for one who has it unset.
    match fragile_entries(&meta.build_env) {
        Ok(fragile) => {
            for (variable, state, value) in fragile {
                findings.push(Finding {
                    severity: Severity::Note,
                    what: format!("with {variable} {state} before it runs, the setup makes it \"{value}\", with an entry that is not an absolute path (an empty one is the working directory)"),
                    from: setup.clone(),
                    fix: Some(format!("write it as export {variable}=\"/dir${{{variable}:+:${variable}}}\"")),
                });
            }
        }
        Err(why) if !gix::interrupt::is_triggered() => findings.push(Finding { severity: Severity::Unchecked, what: why, from: setup.clone(), fix: None }),
        Err(_) => {}
    }
    findings.sort_by_key(|finding| finding.severity);
    findings.dedup();
    Ok(findings)
}

/// The named sets of checks `cache doctor` runs on a configuration whose
/// newest build attempt is `attempt`. One that could not run says so as a
/// finding.
pub fn check_sets(attempt: &crate::build::attempt::BuildAttempt, conf: BuildConf) -> Vec<CheckSet<'_>> {
    vec![CheckSet {
        name: "build environment",
        run: Box::new(move || {
            check_attempt(attempt, conf.clone()).unwrap_or_else(|e| {
                if gix::interrupt::is_triggered() {
                    return Vec::new();
                }
                let machine = &attempt.meta.machine;
                vec![Finding {
                    severity: Severity::Unchecked,
                    what: format!("the build environment could not be checked: {e:#}"),
                    from: format!("the build environment setup of machine {machine} (`cactup machine show {machine}`)"),
                    fix: None,
                }]
            })
        }),
    }]
}

/// `raw` with the flag variable of its language that has the flag its
/// reason is about, where it has none (a reason of the probe's key): a word
/// of the variable the reason names (`-march=native`: "native").
fn with_flag_variable(raw: &Raw, values: &BTreeMap<String, String>) -> Raw {
    let (None, Some(language)) = (&raw.variable, &raw.language) else { return raw.clone() };
    let Some(language) = languages().into_iter().find(|known| known.name == *language) else { return raw.clone() };
    let variable = language.flag_variables.iter().chain([&language.compiler_variable]).find(|variable| {
        let skip = usize::from(**variable == language.compiler_variable);
        values.get(*variable).is_some_and(|value| {
            value.split_whitespace().skip(skip).any(|flag| {
                raw.reason.split([' ', ',', '(', ')']).any(|word| word == flag) || (flag.ends_with("native") && raw.reason.contains("native"))
            })
        })
    });
    Raw { variable: variable.cloned(), ..raw.clone() }
}

/// Where a finding comes from, and what to change.
fn trace(raw: &Raw, reply: &Reply, optionlist: &BTreeMap<String, String>, defaults: &BTreeMap<String, String>, source: &str, setup: &str) -> (String, Option<String>) {
    // A thorn's own flag: only that thorn's compiles get it.
    if let Some(thorn) = &raw.thorn {
        let from = match &raw.variable {
            Some(variable) => format!("thorn {thorn}: its compiles get it in {variable}, the configuration's do not"),
            None => format!("thorn {thorn}"),
        };
        return (from, raw.fix.clone());
    }
    // About an environment variable: the user's shell if it had the
    // variable (with the entries found) before the setup ran, else the
    // setup.
    let variable = raw.variable.clone().or_else(|| raw.reason.split_whitespace().next().filter(|word| word.chars().all(|c| c.is_ascii_uppercase() || c == '_')).map(str::to_owned));
    if let Some(variable) = variable.as_deref().filter(|variable| reply.env.contains_key(*variable) || PATH_VARIABLES.contains(variable) || environment::REFUSED.contains(variable) || *variable == "LD_AUDIT") {
        // The shell's where the setup left the value as the shell had it,
        // or kept entries of the shell's (a non-empty value) it found.
        let inner = reply.env.get(variable).cloned().unwrap_or_default();
        let outer = std::env::var(variable).ok();
        let not_absolute = |value: &str| value.split(':').filter(|entry| !entry.starts_with('/')).map(str::to_owned).collect::<Vec<_>>();
        let from_shell = match &outer {
            Some(outer) if *outer == inner => true,
            Some(outer) if raw.reason.contains("is set") || outer.is_empty() => false,
            Some(outer) => {
                let theirs = not_absolute(outer);
                not_absolute(&inner).iter().all(|entry| theirs.contains(entry))
            }
            None => false,
        };
        let from = match from_shell {
            true => format!("your shell ({variable} is so before the build's environment setup runs: your login files, or what you loaded)"),
            false => setup.to_owned(),
        };
        let fix = match () {
            _ if raw.reason.contains("is set") || variable == "LD_AUDIT" => format!("unset {variable} for the build"),
            _ if variable == "LD_PRELOAD" => "name each library LD_PRELOAD loads by its absolute path".to_owned(),
            _ => format!("give {variable} absolute directories only: export {variable}=\"/dir${{{variable}:+:${variable}}}\" adds one without an empty entry"),
        };
        return (from, Some(fix));
    }
    // About a flag: the option list's variable that has it, else Cactus's
    // defaults for the compiler (what configure wrote). The flag is the
    // reason's first word, or the word of the variable it names.
    let compilers = ["CC", "CXX", "F90", "F77", "CUCC"];
    let in_variable = raw.variable.as_ref().and_then(|variable| reply.values.get(variable).map(|value| (variable, value))).and_then(|(variable, value)| {
        // In a compiler variable, its words after the name.
        let skip = usize::from(compilers.contains(&variable.as_str()));
        value.split_whitespace().skip(skip).find(|flag| raw.reason.split([' ', ',', '(', ')']).any(|word| word == *flag) || (flag.ends_with("native") && raw.reason.contains("native")))
    });
    // (Not where the compiler's whole text is the reason: it is no flag's.)
    let in_variable = in_variable.filter(|_| !raw.reason.contains("is not a plain command"));
    if let Some(flag) = raw.reason.split([' ', ',']).next().filter(|word| word.starts_with('-')).or(in_variable) {
        // Within the finding's language where it has one (`C_OPTIMISE_FLAGS`
        // for C, not `CXX_OPTIMISE_FLAGS`).
        let prefix = match raw.language.as_deref() {
            Some("C") => Some("C_"),
            Some("C++") => Some("CXX_"),
            Some("Fortran") => Some("F90_"),
            Some("Fortran 77") => Some("F77_"),
            Some("CUDA") => Some("CUCC_"),
            _ => None,
        };
        let own = |name: &str| prefix.is_none_or(|prefix| name.starts_with(prefix) || raw.variable.as_deref() == Some(name) || name == "CPPFLAGS");
        let has = |values: &BTreeMap<String, String>| {
            values
                .iter()
                .filter(|(name, _)| own(name))
                .find(|(_, value)| value.split_whitespace().any(|word| word == flag || word.starts_with(&format!("{flag}="))))
                .map(|(name, _)| name.clone())
        };
        let from = match (has(optionlist), has(defaults)) {
            (Some(name), _) => format!("{source}, {name}"),
            (None, Some(name)) => format!("Cactus's default {name} for this compiler (configure wrote it to config-data/make.config.defn; the option list can set {name} itself)"),
            (None, None) => format!("{source} or Cactus's defaults, {}", raw.variable.as_deref().unwrap_or("the flags")),
        };
        let fix = hint(&raw.reason).map(str::to_owned).or_else(|| Some(format!("leave {flag} out, if the build does not need it")));
        return (from, fix);
    }
    // About the compiler: the option list's, else the one configure chose.
    if let Some(variable) = raw.variable.as_deref() {
        let value = reply.values.get(variable).cloned().unwrap_or_default();
        let from = match optionlist.contains_key(variable) {
            true => format!("{source}, {variable} = {value}"),
            false => format!("Cactus's default {variable} = {value} (configure chose it; the option list can set {variable} itself)"),
        };
        return (from, hint(&raw.reason).map(str::to_owned));
    }
    ("the build environment".to_owned(), hint(&raw.reason).map(str::to_owned))
}

/// What would let compiles kept out for `why` in, where cactup knows.
pub fn hint(why: &str) -> Option<&'static str> {
    let hints: [(&str, &str); 11] = [
        ("is not a plain command naming a program", "name the compiler alone in the option list (CC = gcc, CXX = g++, and so on): settings such as LANG=C belong in the machine's environment setup, and flags in the flags variables"),
        ("already stands in front of the compiler", "name the compiler itself in the option list (CC = gcc, not ccache gcc): the build cache takes ccache's place"),
        ("--objdir-as-tempdir", "give nvcc --objdir-as-tempdir in the option list's CUDA flags"),
        ("records when each source was last changed", "take -G and -lineinfo out of the option list's CUDA flags for builds the cache is to serve (a debug build of CUDA code is not cached)"),
        ("native targets the GPU", "name the GPU architecture in the option list instead of native (-arch=sm_80, say)"),
        ("is not a flag the cache knows", "if the flag is not needed, take it out of the option list; if it is, the cache does not know it yet"),
        ("changes the compiler's flags behind its command line", "unset that variable for the build (your shell, or the machine's environment setup)"),
        ("Disk quota exceeded", "a quota is used up (on the build cache's file system or the build's): make room there (for the cache, cactup cache gc), or set build-cache-size to be told before"),
        ("No space left on device", "a file system is full (the build cache's or the build's): make room there (for the cache, cactup cache gc), or set build-cache-size to be told before"),
        ("not published: Failed to create", "the build could not write in the build cache: look at its permissions and free space (cactup cache stats names it)"),
        ("it targets the processor it runs on", "name the processor in the option list instead of -march=native, or build on hosts whose processors are all alike"),
    ];
    hints.iter().find(|(pattern, _)| why.contains(pattern)).map(|(_, hint)| *hint)
}

/// The values of a Cactus option list (`NAME = value` lines).
fn optionlist_values(path: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .filter(|(name, _)| !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .collect()
}

/// The variables configure wrote for the configuration at `config_dir`
/// (`export NAME = value` in `config-data/make.config.defn`), as written.
fn configured_values(config_dir: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(config_dir.join("config-data/make.config.defn"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.strip_prefix("export "))
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conf(tmp: &Path) -> BuildConf {
        BuildConf {
            mode: Mode::Record,
            cactup: PathBuf::from("/opt/cactup"),
            config_dir: tmp.join("configs/sim"),
            cactus_root: tmp.to_owned(),
            machine: "test".into(),
            universe: None,
            build_env_digest: String::new(),
            store: PathBuf::from("/nonexistent/cache"),
            relocate: true,
            exact_warnings: false,
            size_limit: None,
        }
    }

    /// The checks of a language find each variable's refused flag, the
    /// compiler that is not there, and a probe that keys; each finding is
    /// traced to the option list or to Cactus's defaults.
    #[test]
    fn finds_refused_flags_by_variable_and_traces_them() {
        if identity::find_program(OsStr::new("gcc")).is_err() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let c: Vec<Language> = languages().into_iter().filter(|language| ["C", "CUDA", "Fortran 77"].contains(&language.name.as_str())).collect();
        let optionlist: BTreeMap<String, String> = [
            ("CC", "gcc"),
            // One word to the shell: no second input file.
            ("CPPFLAGS", "-DX=1 -DNAME=\"a b\""),
            ("CFLAGS", "-g -O2 -fprofile-generate"),
            ("CUCC", "no-such-nvcc-here"),
            ("F77", "gfortran"),
            ("F77FLAGS", "-g -fprofile-generate"),
        ]
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .into();
        let request = Request { config_dir: None, optionlist: optionlist.clone(), conf: conf(tmp.path()), languages: c };
        let reply = run(&request, false);
        let refused: Vec<&Raw> = reply.found.iter().filter(|raw| raw.severity == Severity::KeepsOut && raw.language.as_deref() == Some("C")).collect();
        assert!(refused.iter().any(|raw| raw.variable.as_deref() == Some("CFLAGS") && raw.reason.starts_with("-fprofile-generate")), "{:?}", reply.found);
        assert!(!refused.iter().any(|raw| raw.variable.as_deref() == Some("CPPFLAGS")), "{:?}", reply.found);
        assert!(reply.found.iter().any(|raw| raw.language.as_deref() == Some("CUDA") && raw.reason.contains("not on PATH")), "{:?}", reply.found);
        if identity::find_program(OsStr::new("gfortran")).is_ok() {
            assert!(reply.found.iter().any(|raw| raw.language.as_deref() == Some("Fortran 77") && raw.variable.as_deref() == Some("F77FLAGS")), "{:?}", reply.found);
        }
        let raw = refused.iter().find(|raw| raw.variable.as_deref() == Some("CFLAGS")).unwrap();
        let defaults: BTreeMap<String, String> = [("C_OPTIMISE_FLAGS".to_owned(), "-O3".to_owned())].into();
        let (from, fix) = trace(raw, &reply, &optionlist, &defaults, "the option list (x)", "the setup");
        assert_eq!(from, "the option list (x), CFLAGS");
        assert!(fix.is_some());
        // A flag written in the compiler variable is the compiler
        // variable's, not each flag variable's.
        let c_only: Vec<Language> = languages().into_iter().filter(|language| language.name == "C").collect();
        let in_cc: BTreeMap<String, String> = [("CC", "gcc --coverage"), ("CFLAGS", "-O2")].map(|(name, value)| (name.to_owned(), value.to_owned())).into();
        let reply_cc = run(&Request { config_dir: None, optionlist: in_cc, conf: conf(tmp.path()), languages: c_only }, false);
        let refused: Vec<&Raw> = reply_cc.found.iter().filter(|raw| raw.severity == Severity::KeepsOut).collect();
        assert!(refused.iter().any(|raw| raw.variable.as_deref() == Some("CC") && raw.reason.contains("--coverage")), "{:?}", reply_cc.found);
        assert!(!refused.iter().any(|raw| raw.variable.as_deref() == Some("CFLAGS")), "{:?}", reply_cc.found);
        // A compiler text only a shell runs: its own hint, no flag's.
        let not_plain = Raw { variable: Some("CC".into()), reason: "LANG=C gcc is not a plain command naming a program, so the recipe's shell runs it".into(), ..(*raw).clone() };
        let with_lang: BTreeMap<String, String> = [("CC".to_owned(), "LANG=C gcc".to_owned())].into();
        let reply_lang = Reply { values: with_lang.clone(), ..Reply::default() };
        let (_, fix) = trace(&not_plain, &reply_lang, &with_lang, &BTreeMap::new(), "x", "y");
        assert!(fix.is_some_and(|fix| fix.starts_with("name the compiler alone")));
        // A flag configure added: Cactus's defaults.
        let from_defaults = Raw { reason: "-O3 is a flag the cache does not follow".into(), ..(*raw).clone() };
        assert!(trace(&from_defaults, &reply, &BTreeMap::new(), &defaults, "x", "y").0.starts_with("Cactus's default C_OPTIMISE_FLAGS"));
    }

    /// The setup runs as the build script runs it: an empty one or one of
    /// comments runs, what it prints is not the answer, one that fails
    /// stops there.
    #[test]
    fn a_setup_runs_as_the_build_script_runs_it() {
        let run = |env: &str| Command::new("/bin/sh").arg("-c").arg(setup_then(env, "echo answer")).output().unwrap();
        for env in ["", "# nothing but a comment", "echo banner; export X=1"] {
            let out = run(env);
            assert_eq!((out.status.success(), String::from_utf8_lossy(&out.stdout).as_ref()), (true, "answer\n"), "{env:?}");
        }
        let out = run("false\nexport X=1");
        assert!(!out.status.success() && out.stdout.is_empty());
        assert!(fragile_entries("").unwrap().is_empty() && fragile_entries("# only this").unwrap().is_empty());
        assert!(fragile_entries("export LD_PRELOAD=libc.so.6\n").unwrap().is_empty(), "a name the loader searches for");
        assert_eq!(fragile_entries("export LD_PRELOAD=lib/x.so\n").unwrap()[0].0, "LD_PRELOAD");
    }

    /// A process the setup leaves in the background does not hold the run
    /// up, nor hide what the setup set; a blank in a directory is no
    /// separator but in `LD_PRELOAD`.
    #[test]
    fn a_setup_that_leaves_a_process_behind_and_a_blank_in_a_path() {
        let start = std::time::Instant::now();
        let found = fragile_entries("sleep 20 &\nexport CPATH=/opt/inc:$CPATH\n").unwrap();
        assert!(start.elapsed() < std::time::Duration::from_secs(10), "{:?}", start.elapsed());
        assert_eq!(found.iter().map(|(variable, _, _)| variable.as_str()).collect::<Vec<_>>(), ["CPATH"]);
        assert!(fragile_entries("export PATH=\"/opt/My Tools/bin:$PATH\"\n").unwrap().is_empty());
    }

    /// What a thorn's compiles get beyond the configuration's is checked,
    /// as Cactus's `make.subdir` reads it: its `make.code.deps` too, its
    /// `SUBDIRS` (a link among them), a capability its bindings include;
    /// a directory it does not compile is not; each finding is traced to
    /// the file that has the flag.
    #[test]
    fn what_a_thorn_s_compiles_get_is_checked() {
        if identity::find_program(OsStr::new("gcc")).is_err() || identity::find_program(OsStr::new("make")).is_err() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let config = root.join("configs/sim");
        let write = |path: PathBuf, text: &str| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        // Cactus's `make.subdir` and compile rules, their shapes as they are.
        write(
            root.join("lib/make/make.subdir"),
            "include $(CONFIG)/make.config.defn\nBINDINGS_DIR=$(TOP)/bindings\n-include $(BINDINGS_DIR)/Configuration/Thorns/make.$(THORN).defn\n-include $(SRCDIR)/make.code.defn\n-include $(TOP)/bindings/Configuration/$(THORN)/make.configuration.defn\nOBJS = $(SRCS:%=%.o)\n$(CCTK_TARGET): $(OBJS) $(SRCDIR)/make.code.defn\n\techo > make.checked\ninclude $(CONFIG)/make.config.rules\n-include $(SRCDIR)/make.code.deps\nifneq ($(strip $(SRCS)),)\n-include $(SRCS:%=%.d)\nendif\n",
        );
        write(config.join("config-data/make.config.defn"), "export CC = gcc\nexport CPPFLAGS =\nexport CFLAGS = -O2\nF90 = none\n");
        write(
            config.join("config-data/make.config.rules"),
            &format!("define COMPILE_C\n$(CC) $(CPPFLAGS) $(CFLAGS) -c $< -o $@\nendef\n%.c.o: $(SRCDIR)/%.c\n\t$(COMPILE_C)\n%.c.d: $(SRCDIR)/%.c\n\t+touch {}/dependencies-made\n", root.display()),
        );
        let thorns = ["Plain", "Deps", "Sub", "Cap", "Tgt", "Pat", "Inc", "Objs", "Top", "Cond", "Stem", "Ind", "Priv", "Run", "Shell", "Rem", "Dup"];
        write(config.join("ThornList"), &format!("# thorns\n{}\n", thorns.map(|thorn| format!("Arr/{thorn}")).join(" # its own\n")));
        let thorn = |name: &str, defn: &str, deps: Option<&str>| {
            let src = root.join("arrangements/Arr").join(name).join("src");
            write(src.join("make.code.defn"), defn);
            if let Some(deps) = deps {
                write(src.join("make.code.deps"), deps);
            }
            for source in ["a.c", "b.c", "c.c", "d.c"] {
                write(src.join(source), "int f(void) { return 0; }\n");
            }
        };
        // Make files the fragment stands down for.
        thorn("Inc", "SRCS = d.c\ninclude $(SRCDIR)/more.mk\n", None);
        write(root.join("arrangements/Arr/Inc/src/more.mk"), "");
        // A flag for one object only, and for objects of a pattern.
        thorn("Tgt", "SRCS = a.c b.c\n", Some("b.c.o: CFLAGS += --coverage\n"));
        thorn("Pat", "SRCS = c.c\n%.c.o: CFLAGS += -fbranch-probabilities\n", None);
        thorn("Plain", "SRCS = a.c\n", None);
        thorn("Deps", "SRCS = a.c\n", Some("CFLAGS += --coverage\n"));
        thorn("Sub", "SRCS = a.c\nSUBDIRS = linked\n", None);
        write(root.join("elsewhere/make.code.defn"), "SRCS = b.c\nCFLAGS += -fprofile-generate\n");
        write(root.join("elsewhere/b.c"), "int g(void) { return 0; }\n");
        std::os::unix::fs::symlink(root.join("elsewhere"), root.join("arrangements/Arr/Sub/src/linked")).unwrap();
        // Not in SUBDIRS: never compiled.
        write(root.join("arrangements/Arr/Sub/src/include/make.code.defn"), "CFLAGS += -fprofile-use\n");
        thorn("Cap", "SRCS = a.c\n", None);
        write(config.join("bindings/Configuration/Thorns/make.Cap.defn"), &format!("include {}/bindings/Configuration/Capabilities/make.FOO.defn\n", config.display()));
        write(config.join("bindings/Configuration/Capabilities/make.FOO.defn"), "CFLAGS += -fauto-profile\n");
        // As the build's sub-make has them: `$(OBJS)` and the goal as
        // targets, sources the configuration chooses, `$*` the stem, a
        // variable a flag variable refers to, `private`.
        thorn("Objs", "SRCS = a.c b.c\n", Some("$(OBJS): CFLAGS += -fprofile-arcs\n"));
        thorn("Top", "SRCS = a.c\n", Some("$(CCTK_TARGET): CFLAGS += -ftest-coverage\n"));
        thorn("Cond", "SRCS = a.c\nifneq ($(strip $(F90)),)\nSRCS += b.c\nendif\n", Some("b.c.o: CFLAGS += -fprofile-update=atomic\n"));
        thorn("Stem", "SRCS = a.c\n", Some("%.c.o: CPPFLAGS += -DOBJ_$*\n"));
        thorn("Ind", "SRCS = a.c\nCFLAGS += $(PROF)\n", Some("a.c.o: PROF = -fprofile-abs-path\n"));
        thorn("Priv", "SRCS = a.c\n", Some("a.c.o: private CFLAGS += -fprofile-dir=x\n"));
        // Nothing is run: neither `+` lines nor `$(MAKE)` (nor the
        // dependency files' remaking); `$(shell)` is, as in a build.
        let ran = root.join("ran");
        thorn("Run", "SRCS = a.c\n", Some(&format!("a.c.o: gen.h\ngen.h:\n\t+touch {}\n\t$(MAKE) -f /dev/null; touch {}\n", ran.display(), ran.display())));
        thorn("Shell", "SRCS = a.c\nCFLAGS += $(shell echo -fprofile-reproducible=serial)\n", None);
        // A flag one object takes away; one a whole directory has, with
        // another flag for one object.
        thorn("Rem", "SRCS = a.c b.c\nCFLAGS += -fprofile-partial-training\n", Some("a.c.o: CFLAGS := -O2\n"));
        thorn("Dup", "SRCS = a.c b.c c.c\nCFLAGS += -fprofile-info-section\n", Some("a.c.o: CFLAGS += -DHARMLESS\n"));
        let c: Vec<Language> = languages().into_iter().filter(|language| language.name == "C").collect();
        let values: BTreeMap<String, String> = [("CC", "gcc"), ("CPPFLAGS", ""), ("CFLAGS", "-O2")].map(|(name, value)| (name.to_owned(), value.to_owned())).into();
        let request = Request { config_dir: Some(config.clone()), optionlist: BTreeMap::new(), conf: BuildConf { cactus_root: root.clone(), ..conf(&root) }, languages: c };
        let work = tempfile::tempdir().unwrap();
        let found = thorn_checks(&request, &config, &values, &[], work.path(), false);
        assert!(!ran.exists() && !root.join("dependencies-made").exists(), "a recipe ran");
        let kept: Vec<&Raw> = found.iter().filter(|raw| raw.severity == Severity::KeepsOut).collect();
        let about = |flag: &str| kept.iter().find(|raw| raw.reason.contains(flag)).and_then(|raw| raw.thorn.clone());
        assert!(about("--coverage").is_some_and(|thorn| thorn.starts_with("Arr/Deps") && thorn.contains("make.code.deps")), "{found:?}");
        assert!(about("-fprofile-generate").is_some_and(|thorn| thorn.starts_with("Arr/Sub")), "{found:?}");
        assert!(about("-fauto-profile").is_some_and(|thorn| thorn.starts_with("Arr/Cap") && thorn.contains("make.FOO.defn")), "{found:?}");
        assert!(kept.iter().find(|raw| raw.reason.contains("-fauto-profile")).and_then(|raw| raw.fix.clone()).is_some_and(|fix| fix.contains("provides the capability")), "{found:?}");
        assert!(about("-fprofile-use").is_none(), "a directory no thorn compiles: {found:?}");
        assert!(!found.iter().any(|raw| raw.thorn.as_deref().is_some_and(|thorn| thorn.starts_with("Arr/Plain"))), "{found:?}");
        let of = |thorn: &str| kept.iter().filter(|raw| raw.thorn.as_deref().is_some_and(|place| place.starts_with(&format!("{thorn} ")))).map(|raw| raw.thorn.clone().unwrap()).collect::<Vec<_>>();
        assert_eq!(of("Arr/Tgt"), ["Arr/Tgt (arrangements/Arr/Tgt/src/make.code.deps, for b.c.o)"], "{found:?}");
        assert_eq!(of("Arr/Pat"), ["Arr/Pat (arrangements/Arr/Pat/src/make.code.defn)"], "{found:?}");
        assert!(kept.iter().any(|raw| raw.thorn.as_deref().is_some_and(|thorn| thorn.starts_with("Arr/Inc")) && raw.reason.contains("line 2 of") && raw.reason.contains("stands down")), "{found:?}");
        assert!(about("-fprofile-arcs").is_some_and(|thorn| thorn.starts_with("Arr/Objs") && !thorn.contains(", for")), "{found:?}");
        assert!(about("-ftest-coverage").is_some_and(|thorn| thorn.starts_with("Arr/Top")), "{found:?}");
        assert!(about("-fprofile-update=atomic").is_some_and(|thorn| thorn.starts_with("Arr/Cond") && thorn.contains("for b.c.o")), "{found:?}");
        assert!(about("-fprofile-abs-path").is_some_and(|thorn| thorn.starts_with("Arr/Ind")), "{found:?}");
        assert!(about("-fprofile-dir=x").is_some_and(|thorn| thorn.starts_with("Arr/Priv")), "{found:?}");
        assert!(about("-fprofile-reproducible=serial").is_some_and(|thorn| thorn.starts_with("Arr/Shell")), "{found:?}");
        assert!(about("-fprofile-partial-training").is_some_and(|thorn| thorn.contains("for b.c.o")), "{found:?}");
        assert_eq!(of("Arr/Dup"), ["Arr/Dup (arrangements/Arr/Dup/src/make.code.defn)"], "{found:?}");
        assert!(of("Arr/Stem").is_empty() && of("Arr/Run").is_empty(), "{found:?}");
        assert!(!found.iter().any(|raw| raw.severity == Severity::Unchecked), "{found:?}");
    }

    /// A setup that gives a variable an empty entry where it was not set
    /// before is found; the idiom that does not, is not.
    #[test]
    fn finds_a_setup_that_gives_an_empty_entry() {
        let found = fragile_entries("export LD_LIBRARY_PATH=/opt/mpi/lib:$LD_LIBRARY_PATH\nexport CPATH=\"${CPATH:+$CPATH:}/opt/inc\"\nmodule load gcc\n").unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!((found[0].0.as_str(), found[0].1, found[0].2.as_str()), ("LD_LIBRARY_PATH", "unset", "/opt/mpi/lib:"));
        assert!(fragile_entries("export PATH=/opt/bin:$PATH\n").unwrap().is_empty(), "PATH is always set");
        assert_eq!(fragile_entries("export LIBRARY_PATH=lib\n").unwrap()[0].0, "LIBRARY_PATH");
        // What the setup prints is not taken for a value; a file it sources
        // that is not there does not hide what it sets after.
        let found = fragile_entries("echo CPATH=:x\necho 'Loading gcc (default=14)'\n. /nonexistent/init/bash\nexport CPATH=/opt/inc:$CPATH\n").unwrap();
        assert_eq!((found.len(), found[0].0.as_str()), (1, "CPATH"), "{found:?}");
        // A setup that stops early says so.
        assert!(fragile_entries("exit 0\n").is_err());
    }

    /// Every machine of the MDB, in every universe: its build environment
    /// setup gives no path variable an empty or relative entry, however the
    /// variable was before (§18.12, M5 part 2: the cache's rules on paths).
    #[test]
    fn no_mdb_machine_build_environment_gives_an_empty_or_relative_entry() {
        let mdb = crate::mdb::Mdb::with_roots(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mdb"), PathBuf::from("/nonexistent-user-mdb"));
        let mut checked = 0;
        let mut bad = Vec::new();
        for (name, _) in mdb.machines().unwrap() {
            let machine = mdb.load(&name).unwrap_or_else(|e| panic!("machine {name} failed to load: {e:#}"));
            let mut universes: Vec<Option<String>> = vec![None];
            universes.extend(machine.meta.universes.keys().cloned().map(Some));
            for universe in universes {
                let env = machine.meta.effective_env(universe.as_deref(), crate::mdb::meta::Phase::Build);
                if env.trim().is_empty() {
                    continue;
                }
                checked += 1;
                match fragile_entries(&env) {
                    Ok(fragile) => {
                        for (variable, state, value) in fragile {
                            bad.push(format!("{name} (universe {universe:?}): with {variable} {state} before, it is \"{value}\""));
                        }
                    }
                    Err(why) => bad.push(format!("{name} (universe {universe:?}): {why}")),
                }
            }
        }
        assert!(bad.is_empty(), "build environment setups that give a path variable an entry that is not an absolute path (write `export X=\"/dir${{X:+:$X}}\"`):\n{}", bad.join("\n"));
        assert!(checked > 20, "only {checked} setups checked");
    }
}
