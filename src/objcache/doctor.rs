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
//! identity's trial compiles run; no object of the build is compiled). It
//! answers in JSON on stdout; `cache doctor` says where each finding comes
//! from (the user's shell, the machine's environment setup, the option
//! list, Cactus's defaults) and what to change ([`Finding`]).
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
            });
            request.optionlist.clone()
        }),
        None => request.optionlist.clone(),
    };
    // The environment: what keeps every compile out.
    for reason in environment::refusals(std::env::vars_os()) {
        let variable = reason.split_whitespace().next().map(str::to_owned);
        reply.found.push(Raw { severity: Severity::KeepsOut, language: None, variable, reason, thorn: None });
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
            });
        }
    }
    let Ok(tmp) = tempfile::tempdir() else { return reply };
    let mut parsers = Vec::new();
    for language in &request.languages {
        reply.found.extend(check_language(request, language, &reply.values, tmp.path(), probe, &mut parsers));
    }
    // What thorns add to the flags for their own compiles.
    if let Some(config_dir) = &request.config_dir {
        reply.found.extend(thorn_flags(config_dir, &request.conf.cactus_root, &reply.values, &parsers));
    }
    reply
}

/// A language whose compiler was identified, and so whose flags can be
/// read.
struct Parser {
    language: Language,
    family: identity::Family,
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

/// What each thorn of the configuration at `config_dir` (a Cactus tree at
/// `cactus_root`) adds to the flag variables for its own compiles, read as
/// Cactus's `make.subdir` reads them for each of the thorn's source
/// directories: the configuration's `make.config.defn`, then the thorn's
/// `bindings/Configuration/Thorns/make.<thorn>.defn`, the directory's
/// `make.code.defn`, and `bindings/Configuration/<thorn>/make.configuration
/// .defn`. Only the flags a thorn adds are checked (the configuration's are
/// checked already), and only directories whose files name a flag variable
/// are asked (`make` runs once for each). A flag set for one target of a
/// thorn (`x.o: CXXFLAGS += …`) is not evaluated: said so.
fn thorn_flags(config_dir: &Path, cactus_root: &Path, values: &BTreeMap<String, String>, parsers: &[Parser]) -> Vec<Raw> {
    let mut found = Vec::new();
    let variables: Vec<String> = {
        let mut variables: Vec<String> = parsers.iter().flat_map(|parser| parser.language.flag_variables.clone()).collect();
        variables.sort();
        variables.dedup();
        variables
    };
    if variables.is_empty() {
        return found;
    }
    let thorns: Vec<String> = std::fs::read_to_string(config_dir.join("ThornList"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.split('#').next())
        .filter_map(|line| line.split_whitespace().next())
        .filter(|thorn| thorn.contains('/'))
        .map(str::to_owned)
        .collect();
    let bindings = config_dir.join("bindings/Configuration");
    for thorn in thorns {
        if gix::interrupt::is_triggered() {
            break;
        }
        let Some(name) = thorn.rsplit('/').next() else { continue };
        let src = cactus_root.join("arrangements").join(&thorn).join("src");
        let shared = [bindings.join(format!("Thorns/make.{name}.defn")), bindings.join(name).join("make.configuration.defn")];
        let mut dirs = Vec::new();
        source_dirs(&src, 0, &mut dirs);
        for dir in dirs {
            let defn = dir.join("make.code.defn");
            let texts: Vec<(PathBuf, String)> = std::iter::once(defn.clone()).chain(shared.iter().cloned()).filter_map(|path| Some((path.clone(), std::fs::read_to_string(&path).ok()?))).collect();
            if !texts.iter().any(|(_, text)| variables.iter().any(|variable| text.contains(variable.as_str()))) {
                continue;
            }
            let shown = defn.strip_prefix(cactus_root).unwrap_or(&defn).display().to_string();
            // A flag variable set for one target only.
            for (path, text) in &texts {
                for line in text.lines().filter(|line| !line.trim_start().starts_with('#')) {
                    let Some((target, rest)) = line.split_once(':') else { continue };
                    let target_like = !target.trim().is_empty() && !target.contains('=') && !rest.starts_with('=');
                    if target_like && variables.iter().any(|variable| rest.trim_start().trim_start_matches("override ").starts_with(variable.as_str())) {
                        found.push(Raw {
                            severity: Severity::Note,
                            language: None,
                            variable: None,
                            reason: format!("a flag variable is set for one target only, which is not checked here: {}", line.trim()),
                            thorn: Some(format!("{thorn} ({})", path.strip_prefix(cactus_root).unwrap_or(path).display())),
                        });
                    }
                }
            }
            let Ok(theirs) = evaluate(config_dir, &variables, Some((name, &dir))) else { continue };
            for parser in parsers {
                for variable in &parser.language.flag_variables {
                    let base: Vec<&str> = values.get(variable).map(|value| value.split_whitespace().collect()).unwrap_or_default();
                    let value = theirs.get(variable).cloned().unwrap_or_default();
                    let words = super::cuda::shell_words(&value, |name| std::env::var(name).ok()).unwrap_or_else(|_| value.split_whitespace().map(str::to_owned).collect());
                    // The thorn's own: the words beyond the configuration's.
                    let mut left = base.clone();
                    let added: Vec<OsString> = words
                        .iter()
                        .filter(|word| match left.iter().position(|base| base == word) {
                            Some(at) => {
                                left.remove(at);
                                false
                            }
                            None => true,
                        })
                        .map(OsString::from)
                        .collect();
                    if added.is_empty() {
                        continue;
                    }
                    if let Err(why) = parse_flags(parser.family, &added, &format!("probe.{}", parser.language.suffix)) {
                        found.push(Raw {
                            severity: Severity::KeepsOut,
                            language: Some(parser.language.name.clone()),
                            variable: Some(variable.clone()),
                            reason: why,
                            thorn: Some(format!("{thorn} ({shown})")),
                        });
                    }
                }
            }
        }
    }
    found
}

/// The directories under `dir` with a `make.code.defn` (a thorn's `src`
/// and those of its subdirectories that have one), to a depth of 4.
fn source_dirs(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    if depth > 4 {
        return;
    }
    if dir.join("make.code.defn").is_file() {
        found.push(dir.to_owned());
    }
    let Ok(listing) = std::fs::read_dir(dir) else { return };
    for entry in listing.flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            source_dirs(&entry.path(), depth + 1, found);
        }
    }
}

/// The compiler and flags of `language`, as `values` has them, checked as a
/// compile of it through the wrapper would be.
fn check_language(request: &Request, language: &Language, values: &BTreeMap<String, String>, tmp: &Path, probe: bool, parsers: &mut Vec<Parser>) -> Vec<Raw> {
    let mut found = Vec::new();
    let raw = |severity, variable: Option<&str>, reason: String| Raw { severity, language: Some(language.name.clone()), variable: variable.map(str::to_owned), reason, thorn: None };
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
    parsers.push(Parser { language: language.clone(), family: compiler.family });
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
    evaluate(config_dir, &variables, None)
}

/// The values of `variables` as `make` has them for the configuration at
/// `config_dir`, and with `thorn` (its name and one of its source
/// directories) as Cactus's `make.subdir` has them for that directory's
/// compiles.
fn evaluate(config_dir: &Path, variables: &[String], thorn: Option<(&str, &Path)>) -> Res<BTreeMap<String, String>> {
    let defn = config_dir.join("config-data/make.config.defn");
    if !defn.is_file() {
        anyhow::bail!("{} is not there", defn.display());
    }
    let dir = tempfile::tempdir().context("Failed to make a temporary directory")?;
    // `$(info)`: no shell between make and the values (a quote in one is
    // its own).
    let mut makefile = format!("TOP := {}\nCONFIG := {}/config-data\ninclude {}\n", config_dir.display(), config_dir.display(), defn.display());
    if let Some((name, src)) = thorn {
        makefile.push_str(&format!(
            "THORN := {name}\nSRCDIR := {src}\nBINDINGS_DIR = $(TOP)/bindings\n-include $(BINDINGS_DIR)/Configuration/Thorns/make.$(THORN).defn\n-include $(SRCDIR)/make.code.defn\n-include $(TOP)/bindings/Configuration/$(THORN)/make.configuration.defn\n",
            src = src.display()
        ));
    }
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
        std::thread::sleep(std::time::Duration::from_millis(50));
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
                    Some(language) if !languages.is_empty() => languages.push(language),
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
            Some(variable) => format!("thorn {thorn}, which adds it to {variable} for its own compiles"),
            None => format!("thorn {thorn}"),
        };
        let fix = (raw.severity == Severity::KeepsOut).then(|| "the cache stays out of that thorn's compiles of this language only; the flag is the thorn's own".to_owned());
        return (from, fix);
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
        ("is not a plain command naming a program", "name the compiler alone in the option list (CC = gcc): settings such as LANG=C belong in the machine's environment setup, and flags in the flags variable"),
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

    /// A flag a thorn adds in its own `make.code.defn` is checked, and
    /// traced to it; the configuration's own flags are not checked again.
    #[test]
    fn a_thorn_s_own_flags_are_checked() {
        if identity::find_program(OsStr::new("gcc")).is_err() || identity::find_program(OsStr::new("make")).is_err() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let config = root.join("configs/sim");
        std::fs::create_dir_all(config.join("config-data")).unwrap();
        std::fs::write(config.join("config-data/make.config.defn"), "export CC = gcc\nexport CPPFLAGS =\nexport CFLAGS = -O2 -fprofile-generate\n").unwrap();
        std::fs::write(config.join("ThornList"), "# thorns\nArr/Plain\nArr/Adds # its own\n").unwrap();
        for (thorn, defn) in [("Plain", "SRCS = a.c\n"), ("Adds", "SRCS = a.c\nCFLAGS += -DTHORN=\"a b\" --coverage\nx.c.o: CFLAGS += -O0\n")] {
            std::fs::create_dir_all(root.join(format!("arrangements/Arr/{thorn}/src"))).unwrap();
            std::fs::write(root.join(format!("arrangements/Arr/{thorn}/src/make.code.defn")), defn).unwrap();
        }
        let c = languages().into_iter().find(|language| language.name == "C").unwrap();
        let values: BTreeMap<String, String> = [("CC", "gcc"), ("CPPFLAGS", ""), ("CFLAGS", "-O2 -fprofile-generate")].map(|(name, value)| (name.to_owned(), value.to_owned())).into();
        let found = thorn_flags(&config, root, &values, &[Parser { language: c, family: identity::Family::Gcc }]);
        let kept: Vec<&Raw> = found.iter().filter(|raw| raw.severity == Severity::KeepsOut).collect();
        assert_eq!(kept.len(), 1, "{found:?}");
        assert!(kept[0].reason.contains("--coverage") && kept[0].thorn.as_deref().is_some_and(|thorn| thorn.starts_with("Arr/Adds")), "{found:?}");
        assert!(found.iter().any(|raw| raw.severity == Severity::Note && raw.reason.contains("one target only")), "{found:?}");
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
