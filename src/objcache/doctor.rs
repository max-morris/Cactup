//! `cactup cache doctor`'s checks of a configuration's build environment
//! (§18.12, M5 part 2): the cache's own rules, applied where its compiles
//! would run, without a build.
//!
//! The checks run in the build's own environment: `cactup cache doctor`
//! starts the attempt's frozen environment setup in a shell, and the shell
//! starts `cactup __cc-doctor` ([`DOCTOR_VERB`]) with what to check on
//! stdin. That process applies to each language's compiler and flags what
//! a compile through the wrapper would ([`run`]): the environment, the
//! driver's own searches, the compiler's identity, the flags (the
//! compile's words read whole; the first reason the wrapper's reader has,
//! decision 32), and a whole key of a small probe compile (the preprocessor and the
//! identity's trial compiles run; no object of the build is compiled), and
//! again where a thorn's compiles get other compilers or flags (read with
//! make through Cactus's own `make.subdir`, [`thorn_checks`]). It answers
//! in JSON on stdout; `cache doctor` says where each finding comes from (the
//! user's shell, the machine's environment setup, the variable of
//! `make.config.defn` make says gives a flag, [`sources`], a thorn's file)
//! and what to change ([`Finding`]).
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
    /// Where the configuration's variable gets the word it is about, as
    /// make says ([`sources`]): the variables whose emptying takes it out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
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
    // Stopped by `cache doctor` on Ctrl-C (a TERM to its process group):
    // what it runs stops, as for the user's Ctrl-C in cactup itself (its
    // temporary files are in a directory `cache doctor` removes).
    // SAFETY: as in `main`, once, before any thread is started.
    let _ = unsafe { gix::interrupt::init_handler(1, || {}) };
    // The build's TMPDIR, as the setup exported it (where the wrapper makes
    // its files for a compile); the checks' own files, and the compilers'
    // they run, go in the directory `cache doctor` gave, which it removes.
    let tmpdir = std::env::var_os("TMPDIR");
    if let Some(work) = std::env::args_os().nth(2) {
        // SAFETY: no other thread is running yet.
        unsafe { std::env::set_var("TMPDIR", work) };
    }
    let mut input = Vec::new();
    let Ok(request) = std::io::stdin().read_to_end(&mut input).map_err(|e| e.to_string()).and_then(|_| serde_json::from_slice::<Request>(&input).map_err(|e| e.to_string())) else {
        return 1;
    };
    // Its own process: the probe's compile may run in its own directory.
    let mut reply = run(&request, true);
    if let Some(value) = &tmpdir {
        reply.env.insert("TMPDIR".to_owned(), value.to_string_lossy().into_owned());
    }
    let scratch = request.config_dir.as_ref().map(|config_dir| config_dir.join("scratch"));
    reply.found.extend(unusable_tmpdir(tmpdir.map(PathBuf::from), scratch.as_deref()));
    // Stopped: no answer, rather than one that looks whole.
    if gix::interrupt::is_triggered() {
        return 1;
    }
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
    // The compilers and flags as make gives them; none where it cannot
    // (not configured: `cache doctor` says so), and then none is checked.
    let made = request.config_dir.as_ref().map(|config_dir| made_values(config_dir, &request.languages));
    let checked = match made {
        Some(Ok(values)) => {
            reply.values = values;
            true
        }
        Some(Err(e)) => {
            reply.found.push(Raw {
                severity: Severity::Unchecked,
                language: None,
                variable: None,
                reason: format!("make could not give the configuration's compilers and flags ({e:#}), so they are not checked, nor what each thorn's own make files give its compiles"),
                thorn: None,
                fix: None,
                sources: Vec::new(),
            });
            false
        }
        None => false,
    };
    // The environment: what keeps every compile out.
    for reason in environment::refusals(std::env::vars_os()) {
        let variable = reason.split_whitespace().next().map(str::to_owned);
        reply.found.push(Raw { severity: Severity::KeepsOut, language: None, variable, reason, thorn: None, fix: None, sources: Vec::new() });
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
                sources: Vec::new(),
            });
        }
    }
    let tmp = match tempfile::tempdir() {
        Ok(tmp) => tmp,
        Err(e) => {
            reply.found.push(Raw {
                severity: Severity::Unchecked,
                language: None,
                variable: None,
                reason: format!("the checks could not make a temporary directory: {e}"),
                thorn: None,
                fix: None,
                sources: Vec::new(),
            });
            return reply;
        }
    };
    // What keeps every compile out whatever its compiler (a relative
    // `COMPILER_PATH`, `GCC_EXEC_PREFIX`), where no compiler is checked.
    if !checked && let Some(why) = key::driver_led_elsewhere(std::ffi::OsStr::new("cc"), false, |variable| std::env::var_os(variable)) {
        let variable = why.split_whitespace().next().filter(|word| word.chars().all(|c| c.is_ascii_uppercase() || c == '_')).map(str::to_owned);
        reply.found.push(Raw { severity: Severity::KeepsOut, language: None, variable, reason: why, thorn: None, fix: None, sources: Vec::new() });
    }
    for language in request.languages.iter().filter(|_| checked) {
        if gix::interrupt::is_triggered() {
            return reply;
        }
        reply.found.extend(check_language(request, language, &reply.values, tmp.path(), probe));
    }
    // Where the configuration's flags get each word a finding is about, as
    // make says it.
    if let Some(config_dir) = request.config_dir.as_ref().filter(|_| checked) {
        for raw in reply.found.iter_mut().filter(|raw| raw.thorn.is_none() && matches!(raw.severity, Severity::KeepsOut | Severity::Costs)) {
            if gix::interrupt::is_triggered() {
                return reply;
            }
            *raw = with_flag_variable(raw, &reply.values);
            let (Some(variable), Some(word)) = (raw.variable.clone(), subject_word(raw, &reply.values)) else { continue };
            raw.sources = sources(config_dir, &variable, &word, !raw.reason.contains(" is read as one)"));
        }
    }
    // What thorns give their own compiles beyond the configuration's.
    if let Some(config_dir) = request.config_dir.as_ref().filter(|_| checked && !gix::interrupt::is_triggered()) {
        let thorns = thorn_checks(request, config_dir, &reply.values, &reply.found, tmp.path(), probe);
        reply.found.extend(thorns);
    }
    reply
}

/// Can the wrapper make its files for a compile in the build's `TMPDIR`
/// (`given`; `None`: not set, so `/tmp`)? It makes one there for the
/// preprocessor's list of what a compile reads, and the recipe's shell for
/// the compiler's lookup: where none can be made, every compile is kept
/// out (and compiles, the compiler falling back on `/tmp`). A relative or
/// empty `TMPDIR` is taken in the directory the compiles run in, `scratch`
/// (Cactus's `SCRATCH_BUILD`: a quoted `~/tmp` is `scratch/~/tmp`); not
/// looked at where there is none yet.
fn unusable_tmpdir(given: Option<PathBuf>, scratch: Option<&Path>) -> Option<Raw> {
    let dir = given.clone().unwrap_or_else(|| PathBuf::from("/tmp"));
    let relative = !dir.is_absolute();
    let place = match relative {
        true => scratch.filter(|scratch| scratch.is_dir())?.join(&dir),
        false => dir.clone(),
    };
    let e = tempfile::Builder::new().prefix(".cactup-doctor-").tempfile_in(&place).err()?;
    let reason = match given {
        Some(_) if relative => format!(
            "TMPDIR is \"{}\", which is no absolute path: the compiles run in {}, where no file can be made in it ({e}), so the build cache keeps every compile out",
            dir.display(),
            scratch.map(Path::display).map(|shown| shown.to_string()).unwrap_or_default()
        ),
        Some(_) => format!("TMPDIR is {}, where no file can be made ({e}), so the build cache keeps every compile out", dir.display()),
        None => format!("TMPDIR is not set and no file can be made in /tmp ({e}), so the build cache keeps every compile out"),
    };
    Some(Raw { severity: Severity::KeepsOut, language: None, variable: Some("TMPDIR".to_owned()), reason, thorn: None, fix: None, sources: Vec::new() })
}

/// Do the flags `flags` pass the wrapper's reader of a compile of a source
/// named `source` with a compiler of `family`? (Without a source, the
/// reader says "there is no input file" unless `flags` have one, and
/// "there is no -o" if they have one but no `-o`.)
fn parse_flags(family: identity::Family, flags: &[OsString], source: Option<&str>) -> Result<(), String> {
    // Without a source, `-c` alone, first: a flag at the end of `flags`
    // that wants a value has nothing after it, rather than taking `-c` for
    // it; and no `-o` of its own, so that a word the reader reads `-o` out
    // of (`-openmp`) is no second one. With an input, the reader then says
    // "there is no -o" (or nothing, where `flags` give one).
    let mut args: Vec<OsString> = match source {
        Some(_) => flags.to_vec(),
        None => std::iter::once(OsString::from("-c")).chain(flags.iter().cloned()).collect(),
    };
    if let Some(source) = source {
        args.extend(["-c", "-o", "probe.o", source].map(OsString::from));
    }
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
    let mut found = Vec::new();
    let tops: Vec<(String, PathBuf)> = thorns.iter().map(|thorn| (thorn.clone(), cactus_root.join("arrangements").join(thorn).join("src"))).filter(|(_, src)| src.is_dir()).collect();
    // A thorn with a `Makefile` of its own is built by it, not by Cactus's.
    let (own, tops): (Vec<_>, Vec<_>) = tops.into_iter().partition(|(_, src)| std::fs::File::open(src.join("Makefile")).is_ok());
    for (thorn, src) in own {
        found.push(Raw {
            severity: Severity::Unchecked,
            language: None,
            variable: None,
            reason: "it is built by a Makefile of its own, not Cactus's: what its compiles get is not checked".to_owned(),
            thorn: Some(format!("{thorn} ({})", src.join("Makefile").strip_prefix(cactus_root).unwrap_or(&src).display())),
            fix: None,
            sources: Vec::new(),
        });
    }
    let uses = uses_thorns(config_dir);
    let tops = crate::par::parallel_map(&tops, |(thorn, src)| (thorn.clone(), src.clone(), made_compiles(request, config_dir, thorn, src, true, &uses)));
    let Ok(tops) = tops else { return Vec::new() };
    let mut dirs: Vec<ThornDir> = Vec::new();
    let mut subdirs = Vec::new();
    for (thorn, src, made) in tops {
        if let Ok(made) = &made {
            subdirs.extend(made.subdirs.iter().map(|sub| (thorn.clone(), src.join(sub))));
        }
        dirs.push((thorn, src, made));
    }
    // And each thorn's bindings, which make.thornlib compiles the same way
    // (for a thorn it builds: with a `src`, and no Makefile of its own).
    for thorn in thorns.iter().filter(|thorn| dirs.iter().any(|(top, _, _)| top == *thorn)) {
        let bindings = config_dir.join("bindings/build").join(thorn.rsplit('/').next().unwrap_or(thorn));
        if bindings.is_dir() {
            subdirs.push((thorn.clone(), bindings));
        }
    }
    let subdirs = crate::par::parallel_map(&subdirs, |(thorn, dir)| (thorn.clone(), dir.clone(), made_compiles(request, config_dir, thorn, dir, false, &uses)));
    let Ok(subdirs) = subdirs else { return Vec::new() };
    dirs.extend(subdirs);

    let mut memo = Memo::new();
    // Each finding, with where (`thorn` is the file, and the objects), and
    // the thorns it is found for: one capability's flag is many thorns'.
    let mut grouped: Vec<(Raw, Vec<String>)> = Vec::new();
    for (thorn, dir, made) in dirs {
        if gix::interrupt::is_triggered() {
            break;
        }
        let shown = dir.strip_prefix(cactus_root).unwrap_or(&dir).display().to_string();
        let unchecked = |reason: String| Raw { severity: Severity::Unchecked, language: None, variable: None, reason, thorn: Some(format!("{thorn} ({shown})")), fix: None, sources: Vec::new() };
        let made = match made {
            Ok(made) => made,
            Err(e) => {
                found.push(unchecked(format!("make could not say what the thorn's compiles there get: {e:#}")));
                continue;
            }
        };
        if made.compiles.is_empty() && made.failed.is_none() {
            // The thorn's `src/make.code.defn`, there but unreadable: the
            // build stops at make.thornlib's `include` of it.
            let defn = dir.join("make.code.defn");
            if dir == cactus_root.join("arrangements").join(&thorn).join("src") && defn.exists() && std::fs::File::open(&defn).is_err() {
                found.push(unchecked(format!("{} cannot be read: a build stops there", defn.strip_prefix(cactus_root).unwrap_or(&defn).display())));
            }
            continue;
        }
        let name = thorn.rsplit('/').next().unwrap_or(&thorn);
        // A directory whose make files might define compile recipes of
        // their own: the fragment stands down there, and nothing is cached.
        if let Some((reason, fix)) = stands_down(&dir, cactus_root) {
            found.push(Raw { severity: Severity::KeepsOut, language: None, variable: None, reason, thorn: Some(format!("{thorn} ({shown})")), fix: Some(fix), sources: Vec::new() });
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
                let fix = thorn_fix(&raw, file.as_deref(), config_dir, own);
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
            let place = match objects.len() {
                n if n == all => format!("({place})"),
                n if n <= 3 => format!("({place}, for {})", objects.join(", ")),
                n => format!("({place}, for {} and {} more)", objects[..2].join(", "), n - 2),
            };
            let raw = Raw { thorn: Some(place), ..raw };
            match grouped.iter_mut().find(|(seen, _)| *seen == raw) {
                Some((_, thorns)) if !thorns.contains(&thorn) => thorns.push(thorn.clone()),
                Some(_) => {}
                None => grouped.push((raw, vec![thorn.clone()])),
            }
        }
    }
    for (raw, thorns) in grouped {
        let names = match thorns.len() {
            n if n <= 6 => thorns.join(", "),
            n => format!("{} and {} more", thorns[..5].join(", "), n - 5),
        };
        let place = raw.thorn.clone().unwrap_or_default();
        found.push(Raw { thorn: Some(format!("{names} {place}")), ..raw });
    }
    found
}

/// Each thorn's `USESTHORNS`, as the build passes it to the thorn's makes
/// (`config-data/make.thornlist`).
fn uses_thorns(config_dir: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(config_dir.join("config-data/make.thornlist"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.strip_prefix("USESTHORNS_"))
        .filter_map(|rest| rest.split_once('='))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect()
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
    // Where the configuration's own check stopped at a reason (only the
    // first is said), what it did not read may show in a thorn's compiles
    // as the next one (a thorn that takes the first out): a reason about a
    // word the configuration's compile has too is the configuration's. Not
    // where its check read all its words.
    let stopped = base.iter().any(|seen| seen.reason == FIRST_ONLY && seen.language.as_ref() == Some(&language.name));
    let configuration: Vec<String> = variables.iter().flat_map(|variable| values.get(*variable).map(|value| value.split_whitespace().map(str::to_owned).collect::<Vec<_>>()).unwrap_or_default()).collect();
    let its_own = |raw: &Raw| !stopped || reason_subject(&raw.reason).is_none_or(|word| !configuration.iter().any(|seen| seen == word));
    again.into_iter().filter(|raw| !already(raw) && its_own(raw)).map(|raw| with_flag_variable(&raw, &merged)).collect()
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
fn made_compiles(request: &Request, config_dir: &Path, thorn: &str, dir: &Path, top: bool, uses: &BTreeMap<String, String>) -> Res<Made> {
    let cactus_root = &request.conf.cactus_root;
    let name = thorn.rsplit('/').next().unwrap_or(thorn);
    let tmp = tempfile::tempdir().context("Failed to make a temporary directory")?;
    // The recipe lines' shell, a script the configuration's shell reads,
    // given that shell's path: no line of a recipe's runs; `$(shell)`'s
    // does, in that shell.
    let shell = tmp.path().join("recipe-shell");
    std::fs::write(&shell, "shell=$1; shift\nif [ -p /dev/stdout ]; then exec \"$shell\" \"$@\"; fi\n").context("Failed to write a script")?;
    if shell.to_string_lossy().contains(char::is_whitespace) {
        anyhow::bail!("the temporary directory's name has a blank in it");
    }
    const OBJECTS: &str = "cactup-doctor-objects ";
    const SUBDIRS: &str = "cactup-doctor-subdirs ";
    const COMPILE: &str = "cactup-doctor-compile ";
    const VALUE: &str = "cactup-doctor-value ";
    let mut print = format!("$(info {OBJECTS}$(OBJS))\n$(info {SUBDIRS}$(SUBDIRS))\n");
    // Cactus's rules for the dependency files make.subdir includes,
    // canceled: none is made (not even by the shell that runs nothing,
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
            // As make.thornlib names it: the thorn's `src` as `src/.`.
            .arg(format!("SRCDIR={}{}", dir.display(), if top { "/." } else { "" }))
            .arg(format!("THORN={name}"))
            .arg(format!("USESTHORNS={}", uses.get(name).map(String::as_str).unwrap_or_default()))
            // (Expanded where it is used: `$(SHELL)` is the configuration's.)
            .arg(format!(".SHELLFLAGS={} $(SHELL) -c", shell.display()))
            // What the build's own makes export to it.
            .env("CCTK_HOME", cactus_root)
            .env("MAKE_DIR", cactus_root.join("lib/make"))
            .env("SCRATCH_BUILD", config_dir.join("scratch"))
            .env_remove("MAKEFILES")
            .env_remove("MAKEFLAGS")
            .env_remove("MAKELEVEL")
            .current_dir(tmp.path()),
        None,
        false,
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

/// Does the fragment stand down for `dir`'s compiles: does `grep -E` find
/// [`super::probe::STAND_DOWN`] in its `make.code.defn` or `make.code.deps`,
/// or fail to read one (the fragment wraps only on grep's "no match")? The
/// finding's reason and fix.
fn stands_down(dir: &Path, cactus_root: &Path) -> Option<(String, String)> {
    // Those that are there as the fragment's `[ -e ]` has it (a link to
    // nothing is not).
    let files: Vec<PathBuf> = [dir.join("make.code.defn"), dir.join("make.code.deps")].into_iter().filter(|file| file.exists()).collect();
    let grep = |args: &[&str]| Command::new("grep").args(["-E"]).args(args).args(["-e", super::probe::STAND_DOWN, "/dev/null"]).args(&files).stdin(Stdio::null()).stderr(Stdio::null()).output();
    let shown = |file: &Path| file.strip_prefix(cactus_root).unwrap_or(file).display().to_string();
    let fix = "the cache stays out of this directory's compiles only. It stands down wherever a thorn's make.code.defn or make.code.deps has an include, a define, an $(eval) or a COMPILE_ name (a comment too), or cannot be read";
    let out = grep(&["-n", "-m", "1"]).ok();
    match out.as_ref().and_then(|out| out.status.code()) {
        Some(1) => None,
        Some(0) => {
            let said = String::from_utf8_lossy(&out.as_ref()?.stdout).into_owned();
            let (file, rest) = said.lines().next()?.split_once(':')?;
            let (number, line) = rest.split_once(':')?;
            let file = shown(Path::new(file));
            Some((
                format!("line {number} of {file} (\"{}\") might define compile recipes of the directory's own, so the build cache stands down for its compiles", line.trim()),
                format!("{fix}: the thorn's to change, if {file} can do without it"),
            ))
        }
        _ => {
            let unread: Vec<String> = files.iter().filter(|file| std::fs::File::open(file).is_err()).map(|file| shown(file)).collect();
            let what = match unread.is_empty() {
                true => "its make files could not be searched (grep failed)".to_owned(),
                false => format!("{} cannot be read", unread.join(" and ")),
            };
            Some((format!("{what}, so the build cache stands down for its compiles (make passes such a file over)"), format!("{fix}: let the file be read")))
        }
    }
}

/// What to change for a flag a thorn gives its compiles, set in `file`
/// where one was found: the thorn's file, or what generates it.
fn thorn_fix(raw: &Raw, file: Option<&Path>, config_dir: &Path, values: &BTreeMap<String, String>) -> Option<String> {
    let flag = shown_for(&raw.reason, raw.variable.as_ref().and_then(|variable| values.get(variable)));
    let leave = flag.map(|flag| format!(" (leave {flag} out, if its compiles do not need it)")).unwrap_or_default();
    let generated = file.is_some_and(|file| file.starts_with(config_dir.join("bindings")));
    let capability = file.is_some_and(|file| file.starts_with(config_dir.join("bindings/Configuration/Capabilities")));
    let r#where = match (file, capability, generated) {
        (None, _, _) => "its make files make it (make.code.defn, make.code.deps, or what they include), maybe of a variable the option list can set too".to_owned(),
        (Some(_), true, _) => "it comes with a capability: the file named is written by the thorn that provides the capability (an ExternalLibraries thorn takes it from its configuration, or from the option list's variables for that library), so change it there".to_owned(),
        (Some(_), false, true) => "the file named is written from the thorn's configuration.ccl and what it requires: change it there".to_owned(),
        (Some(_), false, false) => "the file named sets it: the thorn's to change, not the option list's".to_owned(),
    };
    match raw.severity {
        Severity::KeepsOut => Some(format!("the cache stays out of these compiles only; {where}{leave}")),
        Severity::Costs => Some(format!("{where}{leave}")),
        Severity::Unchecked | Severity::Note => None,
    }
}

/// The file of a thorn's that has the flag `raw` is about: the directory's
/// `make.code.defn` or `make.code.deps`, the thorn's bindings, or a
/// capability those include; none where no file names it.
fn flag_file(raw: &Raw, theirs: &BTreeMap<String, String>, dir: &Path, config_dir: &Path, name: &str) -> Option<PathBuf> {
    let variable = raw.variable.as_ref()?;
    let value = theirs.get(variable)?;
    let flag = value.split_whitespace().find(|flag| names_flag(&raw.reason, flag))?;
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
    let raw = |severity, variable: Option<&str>, reason: String| Raw { severity, language: Some(language.name.clone()), variable: variable.map(str::to_owned), reason, thorn: None, fix: None, sources: Vec::new() };
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
    // The configuration's shell, which runs the recipe (else bash, as
    // Cactus's configure mostly finds it).
    let configured = values.get("SHELL").map(|shell| Path::new(shell.trim())).filter(|shell| shell.is_absolute() && shell.is_file());
    let shell = configured.or_else(|| ["/bin/bash", "/bin/sh"].into_iter().map(Path::new).find(|shell| shell.is_file())).unwrap_or(Path::new("/bin/sh"));
    if let Err(why) = super::lookup::shell_runs_program(&cc_dir, shell, &name) {
        found.push(raw(Severity::KeepsOut, Some(&language.compiler_variable), format!("{why}, so the recipe's shell runs it")));
        return found;
    }
    let fortran = language.name.starts_with("Fortran");
    if let Some(why) = key::driver_led_elsewhere(&name, fortran, |variable| std::env::var_os(variable)) {
        found.push(raw(Severity::KeepsOut, None, why));
    }
    if gix::interrupt::is_triggered() {
        return found;
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
    let source = format!("probe.{}", language.suffix);
    let parse = |flags: &[OsString], with_source: bool| parse_flags(compiler.family, flags, with_source.then_some(source.as_str()));
    // The compile's words as the recipe has them, read whole: the
    // compiler's own after its name (`g++ -std=c++17`), then each variable
    // of flags. What the reader says of the compile as a whole (nvcc's
    // `--objdir-as-tempdir`, a source that is not CUDA) holds of all of
    // them together, not of one; its reason is each variable's that has
    // the word it is about (else the language's flags': where the word
    // would go).
    let mut parts: Vec<(&String, Vec<OsString>)> = vec![(&language.compiler_variable, words[1..].to_vec())];
    for variable in &language.flag_variables {
        // As the recipe's shell splits them (`-DNAME="a b"` is one word).
        let value = values.get(variable).cloned().unwrap_or_default();
        let flags: Vec<OsString> = match super::cuda::shell_words(&value, |name| std::env::var(name).ok()) {
            Ok(words) => words.into_iter().map(OsString::from).collect(),
            // Shell syntax the recipe's shell would expand first (`echo`
            // in backquotes): what the compile gets is not known here.
            Err(_) => {
                found.push(raw(Severity::Unchecked, Some(variable), format!("{variable} has shell syntax the recipe's shell expands before the compiler runs (a command substitution, a ~ or a pattern), which this check does not, so its flags are not checked")));
                continue;
            }
        };
        parts.push((variable, flags));
    }
    let every: Vec<OsString> = parts.iter().flat_map(|(_, words)| words.iter().cloned()).collect();
    if let Some((why, at)) = first_reason(parse, &every) {
        // The variable of the very word, where the reason is about that
        // occurrence only (a second input: the same word may be a value
        // elsewhere, `-isystem /dir`); else every variable with the word,
        // as each copy of a refused flag, or of a word read as `-o`, is.
        let about = reason_subject(&why);
        let mut owners: Vec<&String> = match at {
            Some(at) => {
                let mut start = 0;
                parts.iter().filter(|(_, words)| {
                    let here = (start..start + words.len()).contains(&at);
                    start += words.len();
                    here
                }).map(|(variable, _)| *variable).collect()
            }
            None => parts.iter().filter(|(_, words)| about.is_some_and(|about| words.iter().any(|word| *word == about))).map(|(variable, _)| *variable).collect(),
        };
        if owners.is_empty() {
            owners.extend(language.flag_variables.last().or(Some(&language.compiler_variable)));
        }
        for variable in owners {
            found.push(raw(Severity::KeepsOut, Some(variable), why.clone()));
        }
        // The reader stops at its first reason: nothing past it is read,
        // nor the probe's key made (it would stop there too).
        found.push(raw(Severity::Unchecked, None, FIRST_ONLY.to_owned()));
        return found;
    }
    if !probe || gix::interrupt::is_triggered() {
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
    let mut argv: Vec<OsString> = std::iter::once(words[0].clone()).chain(every).collect();
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
        Err(why) if !found.iter().any(|raw| raw.reason.starts_with(&why)) => found.push(raw(Severity::KeepsOut, None, why)),
        Err(_) => {}
    }
    found
}

/// The words a reason of the wrapper's reader is about, as it gives them
/// before what it says of them (`-Wp,-D_FORTIFY_SOURCE=3 is a flag the
/// cache does not follow`): a flag, or a flag and its value; none where
/// the reason is about no flag.
fn reason_words(reason: &str) -> Option<Vec<&str>> {
    const SAYS: [&str; 6] = [", for the host compiler", " is a flag the cache", " is not a flag the cache", " native targets", " records when", " has nothing after it"];
    let at = SAYS.iter().filter_map(|says| reason.find(says)).min()?;
    let mut words: Vec<&str> = reason[..at].split(' ').collect();
    // A value given joined, which the reader says again
    // (`--ptxas-options=-O3 -O3`): the one word.
    if let [flag, value] = words[..]
        && flag.ends_with(&format!("={value}"))
    {
        words.truncate(1);
    }
    words[0].starts_with('-').then_some(words)
}

/// What a reason of the reader's is about, as the variable `value` has
/// it, for "leave … out": the word a flag is read out of (`-o` of
/// `-override-limits`); a flag with the value it takes as the next word,
/// where the reader knows it takes one (`-mllvm -x86-asm-syntax=intel`:
/// the flag alone does not compile); a flag the reader does not know, with
/// a word after it that is no flag (`-L /opt/lib`: the reader takes that
/// word for an input, so it goes too). A flag the reader refuses that takes
/// no value is named alone (`--coverage`, whatever follows it).
fn shown_for(reason: &str, value: Option<&String>) -> Option<String> {
    // The word it says is read as `-o` (`-openmp`), whatever else the
    // variable has that is read so.
    if reason.contains(" is read as -o") {
        return reason_subject(reason).map(str::to_owned);
    }
    let named = reason_words(reason).or_else(|| reason.split([' ', ',']).next().filter(|word| word.starts_with('-')).map(|word| vec![word]))?;
    let given: Vec<&str> = value.map(|value| value.split_whitespace().collect()).unwrap_or_default();
    let flag = named[0];
    if named.len() == 1 && !given.contains(&flag)
        && let Some(word) = given.iter().find(|word| word.starts_with(flag))
    {
        return Some((*word).to_owned());
    }
    let valued = super::compile::NOT_CACHED_WITH_VALUE.contains(&flag);
    let unknown = reason.contains("not a flag the cache knows");
    if named.len() == 1 && (valued || unknown) {
        let each: Vec<String> = given.windows(2).filter(|pair| pair[0] == flag && (valued || !pair[1].starts_with('-'))).map(|pair| format!("{flag} {}", pair[1])).collect();
        if !each.is_empty() {
            return Some(each.join(" and "));
        }
    }
    Some(named.join(" "))
}

/// The word of the flags a reason of [`first_reason`]'s is about: the flag
/// it names, or the word it says is read as an input or as `-o`; none for a
/// reason about the compile as a whole.
fn reason_subject(reason: &str) -> Option<&str> {
    if let Some(at) = reason.find(" is read as") {
        let open = reason[..at].rfind('(')?;
        return Some(&reason[open + 1..at]);
    }
    reason_words(reason).map(|words| words[0])
}

/// Is `flag` what the reason `reason` is about, or a word of it?
fn names_flag(reason: &str, flag: &str) -> bool {
    reason_words(reason).is_some_and(|words| words.contains(&flag)) || reason.split([' ', ',', '(', ')']).any(|word| word == flag) || (flag.ends_with("native") && reason.contains("native"))
}

/// The reader's reason for a word it takes for a second input file.
const SECOND_INPUT: &str = "there is more than one input file";

/// The reader's reason for a compile with no input file.
const NO_INPUT: &str = "there is no input file";

/// The reader's reason for a second `-o`: a word it reads one out of
/// (`-openmp`, `-override-limits`), where the compile has its own.
const SECOND_OUTPUT: &str = "-o is given more than once";

/// The note beside a first reason: nothing past it is read. `cache doctor`
/// says it once, however many compiles have one.
const FIRST_ONLY: &str = "only the first thing the build cache refuses in a compile is said (its reader stops there)";

/// The reader's first reason for `flags`, if it has one, with the position
/// of the one word it is about where it is about that occurrence only (a
/// second input: the same word elsewhere may be a value): where it is a
/// second input, with the word it takes for that input (asked of the
/// reader, without the probe's source and its `-o`, of each longer start of
/// the flags: the first that has an input ends at it, so a value the reader
/// takes apart from its flag, `-I /dir`, is never named); where it is a
/// second `-o`, with the word it reads one out of (`-openmp`).
///
/// Only the first: the wrapper's reader stops there, and a search past it
/// for every reason (taking each flag said out, and reading on) is on the
/// branch `feature/doctor-every-flag` until it passes its review (Max,
/// 2026-10-15).
fn first_reason(parse: impl Fn(&[OsString], bool) -> Result<(), String>, flags: &[OsString]) -> Option<(String, Option<usize>)> {
    let why = parse(flags, true).err()?;
    if why == SECOND_INPUT
        && let Some(end) = (1..=flags.len()).find(|end| match parse(&flags[..*end], false) {
            // With an input, the reader goes on (no `-o`, the input's
            // language, a flag it does not know): any answer but none.
            Err(why) => why != NO_INPUT && why != SECOND_OUTPUT && !why.ends_with("has nothing after it"),
            Ok(()) => true,
        })
    {
        return Some((format!("{why} ({} is read as one)", flags[end - 1].to_string_lossy()), Some(end - 1)));
    }
    // (Of every variable with that word, where it is read as `-o` too: no
    // position.)
    if why == SECOND_OUTPUT
        && let Some(word) = flags.iter().find(|word| word.to_string_lossy().starts_with("-o") && *word != "-o")
    {
        return Some((format!("{why} ({} is read as -o and what follows it)", word.to_string_lossy()), None));
    }
    Some((why, None))
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
    // And the shell the recipes run in.
    variables.push("SHELL".to_owned());
    variables.sort();
    variables.dedup();
    evaluate(config_dir, &variables)
}

/// Where the configuration at `config_dir` gives its `variable` the word
/// `word`, as make says it: each variable assigned in its
/// `make.config.defn` with the word in its text whose emptying (on make's
/// command line) gives `variable` fewer copies of it; and `variable` itself
/// where none does (its own setting), or, where `every_copy` (each copy of
/// the word is the reason: a refused flag, a word read as `-o`; not a word
/// read as a second input, which is one copy), where copies are left with
/// all those emptied. Nothing of Cactus's is assumed (its switches, which
/// flag variable feeds which): make answers.
fn sources(config_dir: &Path, variable: &str, word: &str, every_copy: bool) -> Vec<String> {
    let copies = |overrides: &[String]| evaluate_with(config_dir, &[variable.to_owned()], overrides).ok().and_then(|values| values.get(variable).cloned()).map(|value| value.split_whitespace().filter(|seen| *seen == word).count());
    let Some(all) = copies(&[]).filter(|all| *all > 0) else { return Vec::new() };
    let text = std::fs::read_to_string(config_dir.join("config-data/make.config.defn")).unwrap_or_default();
    let mut names: Vec<String> = text
        .lines()
        .filter(|line| line.contains(word))
        .filter_map(|line| {
            let line = line.trim_start().trim_start_matches("export ").trim_start_matches("override ").trim_start();
            let end = line.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))?;
            let (name, rest) = line.split_at(end);
            let rest = rest.trim_start();
            (!name.is_empty() && name != variable && ["=", ":=", "::=", "+=", "?="].iter().any(|op| rest.starts_with(op))).then(|| name.to_owned())
        })
        .collect();
    names.sort();
    names.dedup();
    let mut found: Vec<String> = names.into_iter().filter(|name| copies(&[format!("{name}=")]).is_some_and(|left| left < all)).collect();
    let emptied: Vec<String> = found.iter().map(|name| format!("{name}=")).collect();
    if found.is_empty() || (every_copy && copies(&emptied).is_some_and(|left| left > 0)) {
        found.push(variable.to_owned());
    }
    found
}

/// The word of its variable a finding is about: the one its reason names
/// (a flag, a word read as an input or as `-o`), else the one of the
/// variable's words the reason speaks of (`-march=native`: "native").
fn subject_word(raw: &Raw, values: &BTreeMap<String, String>) -> Option<String> {
    if let Some(word) = reason_subject(&raw.reason) {
        return Some(word.to_owned());
    }
    let value = values.get(raw.variable.as_ref()?)?;
    let skip = usize::from(["CC", "CXX", "F90", "F77", "CUCC"].contains(&raw.variable.as_deref()?));
    value.split_whitespace().skip(skip).find(|word| names_flag(&raw.reason, word)).map(str::to_owned)
}

/// The values of `variables` as `make` has them for the configuration at
/// `config_dir`.
fn evaluate(config_dir: &Path, variables: &[String]) -> Res<BTreeMap<String, String>> {
    evaluate_with(config_dir, variables, &[])
}

/// [`evaluate`], with `NAME=value` overrides on make's command line.
fn evaluate_with(config_dir: &Path, variables: &[String], overrides: &[String]) -> Res<BTreeMap<String, String>> {
    let defn = config_dir.join("config-data/make.config.defn");
    if !defn.is_file() {
        anyhow::bail!("{} is not there", defn.display());
    }
    let dir = temp_dir()?;
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
        .args(overrides)
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .output()
        .context("Failed to run make")?;
    if !out.status.success() {
        let said = String::from_utf8_lossy(&out.stderr);
        let last = said.lines().rev().find(|line| !line.trim().is_empty()).unwrap_or("it said nothing");
        anyhow::bail!("make could not read {}: {last}", defn.display());
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
        let out = run_to_files(&mut command, None, true).map_err(|e| format!("{e:#}"))?;
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
    // The checks' temporary files (theirs, and the compilers' they run)
    // all go in a directory of this process's, which goes when they are
    // done, however they ended (a Ctrl-C that had to kill them too).
    let work = temp_dir()?;
    let script = setup_then(env, &checks_command(work.path(), cactup));
    let input = serde_json::to_vec(request).context("Failed to write the checks' request")?;
    let out = run_to_files(Command::new("/bin/sh").arg("-c").arg(&script).current_dir(cactus_root), Some(input), true);
    drop(work);
    let out = out?;
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

/// What the shell runs once the setup is through: the line that says so,
/// then the checks, given `work` for their TMPDIR (they read the build's
/// own, as exported, first).
fn checks_command(work: &Path, cactup: &Path) -> String {
    format!("printf '%s\\n' '{SETUP_DONE}' >&2\nexec {} {DOCTOR_VERB} {}", crate::build::sh_quote(cactup), crate::build::sh_quote(work))
}

/// A temporary directory, in TMPDIR, else in /tmp (as the compilers fall
/// back on it: a TMPDIR that is not there is a finding, not a failure).
fn temp_dir() -> Res<tempfile::TempDir> {
    tempfile::tempdir().or_else(|_| tempfile::tempdir_in("/tmp")).with_context(|| format!("Failed to make a temporary directory in {}", tried()))
}

/// Where [`temp_dir`] and [`temp_file`] look: TMPDIR, then /tmp.
fn tried() -> String {
    match std::env::temp_dir() {
        dir if dir == Path::new("/tmp") => "/tmp".to_owned(),
        dir => format!("{} or /tmp", dir.display()),
    }
}

/// What [`temp_dir`] and [`temp_file`] say when they find no room.
const NO_ROOM: &str = "Failed to make a temporary ";

/// A temporary file, as [`temp_dir`] places it.
fn temp_file() -> Res<std::fs::File> {
    tempfile::tempfile().or_else(|_| tempfile::tempfile_in("/tmp")).with_context(|| format!("Failed to make a temporary file in {}", tried()))
}

/// Wait for `child`, stopping it on Ctrl-C (`CLAUDE.md`'s interrupt
/// contract): a module system's setup can take a while.
fn wait_interruptibly(mut child: std::process::Child, group: bool) -> Res<std::process::ExitStatus> {
    // From 1 ms, up to 50: most of what runs here (a make in each of
    // hundreds of directories) is over in a few.
    let mut pause = std::time::Duration::from_millis(1);
    loop {
        if let Some(status) = child.try_wait().context("Failed to wait for the build environment")? {
            return Ok(status);
        }
        if gix::interrupt::is_triggered() {
            // The child's process group, where it has one of its own: what
            // it started too. Asked to stop first (the checks then stop
            // what they run), then made to.
            match rustix::process::Pid::from_raw(child.id() as i32).filter(|_| group) {
                Some(group) => {
                    let _ = rustix::process::kill_process_group(group, rustix::process::Signal::TERM);
                    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
                    while std::time::Instant::now() < deadline && child.try_wait().ok().flatten().is_none() {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
                }
                None => {
                    let _ = child.kill();
                }
            }
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
fn run_to_files(command: &mut Command, input: Option<Vec<u8>>, group: bool) -> Res<std::process::Output> {
    let stdout = temp_file()?;
    let stderr = temp_file()?;
    if group {
        command.process_group(0);
    }
    let mut child = command
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
    let status = wait_interruptibly(child, group)?;
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
    // The configure the newest attempt builds with: the one there, unless
    // the attempt has an option list other than the one installed with it
    // (installed when an attempt succeeds): then it configures again when
    // it runs (a build submitted and queued, or one that failed), and what
    // is there now is not what it compiles with.
    // The option list it was configured with is the one installed with it
    // when an attempt succeeds (else the newest earlier attempt's that
    // succeeded), rendered as the attempt's is: compared as written.
    // (Not Cactus's `config-info`, which records the options with
    // comments taken out and `${VAR}` expanded.)
    let installed = std::fs::read(meta.config_dir.join("cactup-optionlist.cfg")).ok().or_else(|| {
        (0..attempt.id)
            .rev()
            .filter_map(|id| crate::build::attempt::BuildAttempt::open(&meta.config_dir, id).ok())
            .find(|earlier| earlier.meta.outcome.as_ref().is_some_and(|outcome| outcome.complete && outcome.exit_status == Some(0)))
            .and_then(|earlier| std::fs::read(earlier.optionlist_path()).ok())
    });
    let reconfigures = std::fs::read(attempt.optionlist_path()).is_ok_and(|ours| installed.is_some_and(|installed| installed != ours));
    // A configure that failed leaves the one before in place
    // (`CONFIG-STATUS` in `config-info` is not 0): not what is built.
    let failed = configure_failed(&meta.config_dir);
    let configured = meta.config_dir.join("config-data/make.config.defn").is_file() && !reconfigures && !failed;
    if !configured {
        let why = match (reconfigures, failed) {
            (true, _) => "the newest build attempt configures the configuration again, from an option list other than the one it was configured with, when it runs",
            (false, true) => "the configuration's last configure failed, and what it left is not what a build compiles with",
            (false, false) => "the configuration is not configured yet",
        };
        findings.push(Finding { severity: Severity::Unchecked, what: format!("{why}: its compilers and flags are not checked"), from: "the configuration".to_owned(), fix: None });
        findings.push(Finding {
            severity: Severity::Unchecked,
            what: format!("what each thorn's own make files give its compiles is not checked: {why}"),
            from: "the configuration".to_owned(),
            fix: Some("run cactup cache doctor again once a build has configured it (it does, before it compiles)".to_owned()),
        });
    } else if !meta.config_dir.join("bindings/Configuration").is_dir() {
        findings.push(Finding {
            severity: Severity::Unchecked,
            what: "the configuration's bindings are not written yet: what the capabilities a thorn requires give its compiles, and the bindings' own compiles, are not checked".to_owned(),
            from: "the configuration".to_owned(),
            fix: Some("run cactup cache doctor again once a build has written them (a build does, before it compiles)".to_owned()),
        });
    }
    let written = match configured {
        true => written_values(&meta.config_dir),
        false => BTreeMap::new(),
    };
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
    // The note that only the first reason is said: once, however many
    // compiles (and thorns) it is of.
    let first_only = reply.found.iter().any(|raw| raw.reason == FIRST_ONLY);
    if first_only {
        findings.push(Finding {
            severity: Severity::Unchecked,
            what: format!("{FIRST_ONLY}, so what else keeps those compiles out is not checked"),
            from: "the build cache's reader of compiles".to_owned(),
            fix: Some("change what is said, and run cactup cache doctor again".to_owned()),
        });
    }
    let found: Vec<Raw> = reply.found.iter().filter(|raw| raw.reason != FIRST_ONLY).map(|raw| with_flag_variable(raw, &reply.values)).collect();
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
        let (from, fix) = trace(raw, &reply, &optionlist, &written, &source, &setup);
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
                // No room for cache doctor's own files: before the setup ran.
                let from = match format!("{e:#}").starts_with(NO_ROOM) {
                    true => "this host (your shell's TMPDIR, or /tmp)".to_owned(),
                    false => format!("the build environment setup of machine {machine} (`cactup machine show {machine}`)"),
                };
                vec![Finding { severity: Severity::Unchecked, what: format!("the build environment could not be checked: {e:#}"), from, fix: None }]
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
                names_flag(&raw.reason, flag)
            })
        })
    });
    Raw { variable: variable.cloned(), ..raw.clone() }
}

/// Where a finding comes from, and what to change.
fn trace(raw: &Raw, reply: &Reply, optionlist: &BTreeMap<String, String>, written: &BTreeMap<String, String>, source: &str, setup: &str) -> (String, Option<String>) {
    // A thorn's own flag: only that thorn's compiles get it.
    if let Some(thorn) = &raw.thorn {
        let from = match &raw.variable {
            Some(variable) if thorn.split(" (").next().is_some_and(|names| names.contains(", ")) => format!("thorns {thorn}: their compiles get it in {variable}, the configuration's do not"),
            Some(variable) => format!("thorn {thorn}: its compiles get it in {variable}, the configuration's do not"),
            None => format!("thorn {thorn}"),
        };
        return (from, raw.fix.clone());
    }
    // About an environment variable: the user's shell if it had the
    // variable (with the entries found) before the setup ran, else the
    // setup.
    let variable = raw.variable.clone().or_else(|| raw.reason.split_whitespace().next().filter(|word| word.chars().all(|c| c.is_ascii_uppercase() || c == '_')).map(str::to_owned));
    if let Some(variable) = variable.as_deref().filter(|variable| reply.env.contains_key(*variable) || PATH_VARIABLES.contains(variable) || environment::REFUSED.contains(variable) || ["LD_AUDIT", "TMPDIR"].contains(variable)) {
        // The shell's where the setup left the value as the shell had it,
        // or kept entries of the shell's (a non-empty value) it found.
        let inner = reply.env.get(variable).cloned().unwrap_or_default();
        let outer = std::env::var(variable).ok();
        let not_absolute = |value: &str| value.split(':').filter(|entry| !entry.starts_with('/')).map(str::to_owned).collect::<Vec<_>>();
        let from_shell = match &outer {
            Some(outer) if *outer == inner => true,
            Some(outer) if raw.reason.contains("is set") || outer.is_empty() || variable == "TMPDIR" => false,
            Some(outer) => {
                let theirs = not_absolute(outer);
                not_absolute(&inner).iter().all(|entry| theirs.contains(entry))
            }
            None => false,
        };
        let from = match from_shell {
            _ if variable == "TMPDIR" && raw.reason.contains("is not set") => "this host's /tmp".to_owned(),
            true => format!("your shell ({variable} is so before the build's environment setup runs: your login files, or what you loaded)"),
            false => setup.to_owned(),
        };
        let fix = match () {
            _ if variable == "TMPDIR" && raw.reason.contains("is not set") => "set TMPDIR to a directory that is there and writable, for the build".to_owned(),
            _ if variable == "TMPDIR" && raw.reason.contains("no absolute path") => "give TMPDIR an absolute directory (a ~ in quotes is not expanded: write $HOME)".to_owned(),
            _ if variable == "TMPDIR" => "set TMPDIR to a directory that is there and writable for you (or make the one it names)".to_owned(),
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
        value.split_whitespace().skip(skip).find(|flag| names_flag(&raw.reason, flag))
    });
    // (Not where the compiler's whole text is the reason: it is no flag's.)
    let in_variable = in_variable.filter(|_| !raw.reason.contains("is not a plain command"));
    // (A flag with a comma in it is one: `-Wp,-D_FORTIFY_SOURCE=3`.)
    let named = reason_words(&raw.reason);
    // The word it is about (`-openmp` of a second `-o`, the word taken for
    // a second input), where it says one.
    let first = reason_subject(&raw.reason).or(named.as_ref().map(|words| words[0])).or_else(|| raw.reason.split([' ', ',']).next().filter(|word| word.starts_with('-')));
    if let Some(flag) = first.or(in_variable) {
        // Where make says the configuration's variable gets it
        // (`Raw::sources`): the option list's variable, or one configure
        // wrote. Nothing of Cactus's is assumed here.
        // What can be seen, no more: the variable as configure wrote it,
        // and whether the option list has it the same (configure may have
        // written it from another: F77's from F90's).
        let words = |value: &String| value.split_whitespace().collect::<Vec<_>>().join(" ");
        let place = |name: &String| match (optionlist.get(name), written.get(name)) {
            (Some(listed), Some(wrote)) if words(listed) == words(wrote) => format!("{name} (config-data/make.config.defn; the same in {source})"),
            _ => format!("{name} as configure wrote it (config-data/make.config.defn)"),
        };
        let from = match (raw.sources.is_empty(), raw.variable.as_ref()) {
            (false, _) => raw.sources.iter().map(place).collect::<Vec<_>>().join("; "),
            (true, Some(variable)) => place(variable),
            (true, None) => "the configuration's flags".to_owned(),
        };
        let value = raw.variable.as_ref().and_then(|variable| reply.values.get(variable));
        // A flag the reader reads out of a longer word (`-o` of
        // `-override-limits`): that word.
        let whole = value.and_then(|value| {
            let words: Vec<&str> = value.split_whitespace().collect();
            (!words.contains(&flag)).then(|| words.into_iter().find(|word| word.starts_with(flag))).flatten()
        });
        let shown = shown_for(&raw.reason, value).unwrap_or_else(|| flag.to_owned());
        let flag = whole.unwrap_or(flag);
        let fix = match hint(&raw.reason) {
            // An unknown flag with a word after it that is no flag: both.
            Some(_) if raw.reason.contains("is not a flag the cache knows") && shown != flag => {
                Some(format!("if {flag} is not needed, leave {shown} out; if it is, the cache does not know it yet"))
            }
            Some(hint) => Some(hint.to_owned()),
            None => Some(format!("leave {shown} out, if the build does not need it")),
        };
        return (from, fix);
    }
    // About the compiler: the option list's, else the one configure chose.
    if let Some(variable) = raw.variable.as_deref() {
        let value = reply.values.get(variable).cloned().unwrap_or_default();
        let from = match (optionlist.get(variable), written.get(variable)) {
            (Some(listed), Some(wrote)) if listed.split_whitespace().eq(wrote.split_whitespace()) => format!("{variable} = {value} (config-data/make.config.defn; the same in {source})"),
            _ => format!("{variable} = {value} as configure wrote it (config-data/make.config.defn)"),
        };
        return (from, hint(&raw.reason).map(str::to_owned));
    }
    match raw.reason.starts_with("make could not give the configuration's") {
        true => ("the configuration (config-data/make.config.defn)".to_owned(), None),
        false => ("the build environment".to_owned(), hint(&raw.reason).map(str::to_owned)),
    }
}

/// What would let compiles kept out for `why` in, where cactup knows.
pub fn hint(why: &str) -> Option<&'static str> {
    let hints: [(&str, &str); 12] = [
        ("is not a plain command naming a program", "name the compiler alone in the option list (CC = gcc, CXX = g++, and so on): settings such as LANG=C belong in the machine's environment setup, and flags in the flags variables"),
        ("already stands in front of the compiler", "name the compiler itself in the option list (CC = gcc, not ccache gcc): the build cache takes ccache's place"),
        ("--objdir-as-tempdir", "give nvcc --objdir-as-tempdir in the option list's CUDA flags"),
        ("records when each source was last changed", "take -G and -lineinfo out of the option list's CUDA flags for builds the cache is to serve (a debug build of CUDA code is not cached)"),
        ("native targets the GPU", "name the GPU architecture in the option list instead of native (-arch=sm_80, say)"),
        ("is not a flag the cache knows", "if the flag is not needed, leave it out; if it is, the cache does not know it yet"),
        ("is read as one)", "the word named is read as a second source: leave it out, with the flag it belongs to, if the build does not need them (the cache takes a value apart from its flag only for flags it knows: -I, -D, -isystem and the like)"),
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
    // As Cactus's `setup_configuration.pl` reads it: a `#` to the end of
    // the line is a comment, `keyword = value` or `keyword value`, and the
    // first setting of a keyword stands (but an empty one). (Its names are
    // what is used: whether a variable a finding comes from is the option
    // list's.)
    let mut values = BTreeMap::new();
    for line in std::fs::read_to_string(path).unwrap_or_default().lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        let end = line.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(line.len());
        let (name, rest) = line.split_at(end);
        if name.is_empty() || !rest.starts_with(['=', ' ', '\t']) {
            continue;
        }
        let value = rest.trim_start_matches(['=', ' ', '\t']).to_owned();
        let set: &mut String = values.entry(name.to_owned()).or_default();
        if set.is_empty() {
            *set = value;
        }
    }
    values
}

/// The variables configure wrote for the configuration at `config_dir`
/// (`export NAME = value` in `config-data/make.config.defn`), as written:
/// what a finding's variable is said to be.
fn written_values(config_dir: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(config_dir.join("config-data/make.config.defn"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.strip_prefix("export "))
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect()
}

/// Did Cactus's last configure of the configuration at `config_dir` fail
/// (`# CONFIG-STATUS : n` in `config-info`, n not 0)? Not where there is
/// no record.
fn configure_failed(config_dir: &Path) -> bool {
    std::fs::read_to_string(config_dir.join("config-info"))
        .unwrap_or_default()
        .lines()
        .find_map(|line| line.strip_prefix("# CONFIG-STATUS"))
        .and_then(|rest| rest.trim_start_matches([' ', ':']).trim().parse::<i32>().ok())
        .is_some_and(|status| status != 0)
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
    /// compiler that is not there; each finding is traced where make says
    /// the configuration gets it (a switch that is off is no source, as
    /// make has it), to the option list's variable or one configure wrote.
    #[test]
    fn finds_refused_flags_by_variable_and_traces_them() {
        if identity::find_program(OsStr::new("gcc")).is_err() || identity::find_program(OsStr::new("make")).is_err() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let configured = |name: &str, defn: &str| {
            let config = tmp.path().join(name);
            std::fs::create_dir_all(config.join("config-data")).unwrap();
            std::fs::write(config.join("config-data/make.config.defn"), defn).unwrap();
            config
        };
        // As configure writes it: switched flag variables added to CFLAGS
        // in conditionals.
        let config = configured(
            "sim",
            "export SHELL = /bin/sh\nexport CC = gcc\nexport CPPFLAGS = -DX=1 -DNAME=\"a b\"\nexport CFLAGS = -g -O2\n\
             export C_DEBUG_FLAGS = -fprofile-generate\nexport C_OPTIMISE_FLAGS = -O3 -fprofile-generate\n\
             export CCTK_DEBUG_MODE = no\nifeq ($(strip $(CCTK_DEBUG_MODE)),yes)\n  CFLAGS += $(C_DEBUG_FLAGS)\nendif\n\
             export CCTK_OPTIMISE_MODE = yes\nifeq ($(strip $(CCTK_OPTIMISE_MODE)),yes)\n  CFLAGS += $(C_OPTIMISE_FLAGS)\nendif\n\
             export CUCC = no-such-nvcc-here\nexport F90 = gfortran\nexport F90FLAGS = -g -fprofile-generate\nexport F77 = gfortran\nexport F77FLAGS = -g -fprofile-generate\n",
        );
        let c: Vec<Language> = languages().into_iter().filter(|language| ["C", "CUDA", "Fortran 77"].contains(&language.name.as_str())).collect();
        let request = Request { config_dir: Some(config.clone()), optionlist: BTreeMap::new(), conf: conf(tmp.path()), languages: c };
        let reply = run(&request, false);
        let refused: Vec<&Raw> = reply.found.iter().filter(|raw| raw.severity == Severity::KeepsOut && raw.language.as_deref() == Some("C")).collect();
        let raw = refused.iter().find(|raw| raw.variable.as_deref() == Some("CFLAGS") && raw.reason.starts_with("-fprofile-generate")).expect("CFLAGS's flag");
        assert_eq!(raw.sources, ["C_OPTIMISE_FLAGS"], "{:?}", reply.found);
        // A flag from two places: both (the variable's own setting too).
        let two = configured(
            "two",
            "export SHELL = /bin/sh\nexport CC = gcc\nexport CFLAGS = -g -flto\nexport C_OPTIMISE_FLAGS = -O3 -flto\n\
             export CCTK_OPTIMISE_MODE = yes\nifeq ($(strip $(CCTK_OPTIMISE_MODE)),yes)\n  CFLAGS += $(C_OPTIMISE_FLAGS)\nendif\n",
        );
        assert_eq!(sources(&two, "CFLAGS", "-flto", true), ["C_OPTIMISE_FLAGS", "CFLAGS"]);
        // A word read as a second input is one copy: not the variable's
        // own where another variable gives it.
        assert_eq!(sources(&two, "CFLAGS", "-flto", false), ["C_OPTIMISE_FLAGS"]);
        assert!(!refused.iter().any(|raw| raw.variable.as_deref() == Some("CPPFLAGS")), "{:?}", reply.found);
        assert!(reply.found.iter().any(|raw| raw.language.as_deref() == Some("CUDA") && raw.reason.contains("not on PATH")), "{:?}", reply.found);
        if identity::find_program(OsStr::new("gfortran")).is_ok() {
            assert!(reply.found.iter().any(|raw| raw.language.as_deref() == Some("Fortran 77") && raw.reason.starts_with("-fprofile-generate") && raw.sources == ["F77FLAGS"]), "{:?}", reply.found);
        }
        let optionlist: BTreeMap<String, String> = [("C_OPTIMISE_FLAGS".to_owned(), "-O3 -fprofile-generate".to_owned())].into();
        let written = written_values(&config);
        let (from, fix) = trace(raw, &reply, &optionlist, &written, "the option list (x)", "the setup");
        assert_eq!(from, "C_OPTIMISE_FLAGS (config-data/make.config.defn; the same in the option list (x))");
        assert!(fix.is_some());
        let (from, _) = trace(raw, &reply, &BTreeMap::new(), &written, "the option list (x)", "the setup");
        assert_eq!(from, "C_OPTIMISE_FLAGS as configure wrote it (config-data/make.config.defn)");
        // The option list has another: not said to be where it comes from.
        let other: BTreeMap<String, String> = [("C_OPTIMISE_FLAGS".to_owned(), "-O1 -fprofile-generate -g".to_owned())].into();
        assert_eq!(trace(raw, &reply, &other, &written, "x", "y").0, "C_OPTIMISE_FLAGS as configure wrote it (config-data/make.config.defn)");
        // A flag written in the compiler variable is the compiler
        // variable's, not each flag variable's.
        let c_only: Vec<Language> = languages().into_iter().filter(|language| language.name == "C").collect();
        let in_cc = configured("cc", "export SHELL = /bin/sh\nexport CC = gcc --coverage\nexport CFLAGS = -O2\n");
        let reply_cc = run(&Request { config_dir: Some(in_cc), optionlist: BTreeMap::new(), conf: conf(tmp.path()), languages: c_only.clone() }, false);
        let refused: Vec<&Raw> = reply_cc.found.iter().filter(|raw| raw.severity == Severity::KeepsOut).collect();
        assert!(refused.iter().any(|raw| raw.variable.as_deref() == Some("CC") && raw.reason.contains("--coverage")), "{:?}", reply_cc.found);
        assert!(!refused.iter().any(|raw| raw.variable.as_deref() == Some("CFLAGS")), "{:?}", reply_cc.found);
        // Not configured: no compiler or flag is checked.
        let reply_none = run(&Request { config_dir: None, optionlist: [("CFLAGS".to_owned(), "--coverage".to_owned())].into(), conf: conf(tmp.path()), languages: c_only }, false);
        assert!(!reply_none.found.iter().any(|raw| raw.language.is_some()), "{:?}", reply_none.found);
        // A compiler text only a shell runs: its own hint, no flag's.
        let not_plain = Raw { variable: Some("CC".into()), reason: "LANG=C gcc is not a plain command naming a program, so the recipe's shell runs it".into(), ..(*raw).clone() };
        let with_lang: BTreeMap<String, String> = [("CC".to_owned(), "LANG=C gcc".to_owned())].into();
        let reply_lang = Reply { values: with_lang.clone(), ..Reply::default() };
        let (_, fix) = trace(&not_plain, &reply_lang, &with_lang, &BTreeMap::new(), "x", "y");
        assert!(fix.is_some_and(|fix| fix.starts_with("name the compiler alone")));
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
        // The configuration's shell is bash, as Cactus's configure makes it.
        write(config.join("config-data/make.config.defn"), "export SHELL = /bin/bash\nexport CC = gcc\nexport CPPFLAGS =\nexport CFLAGS = -O2\nF90 = none\n");
        write(config.join("config-data/make.thornlist"), "USESTHORNS_Uses = Foo\nUSESTHORNS_Plain =\n");
        write(
            config.join("config-data/make.config.rules"),
            &format!("define COMPILE_C\n$(CC) $(CPPFLAGS) $(CFLAGS) -c $< -o $@\nendef\n%.c.o: $(SRCDIR)/%.c\n\t$(COMPILE_C)\n%.c.d: $(SRCDIR)/%.c\n\t+touch {}/dependencies-made\n", root.display()),
        );
        let thorns = ["Plain", "Deps", "Sub", "Cap", "Cap2", "Tgt", "Pat", "Inc", "Objs", "Top", "Cond", "Stem", "Ind", "Priv", "Run", "Shell", "Rem", "Dup", "Bash", "Uses", "Unread", "Own", "Dangle", "Two", "Bnd", "Mllvm"];
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
        // Two thorns that require it: one finding.
        thorn("Cap2", "SRCS = a.c\n", None);
        write(config.join("bindings/Configuration/Thorns/make.Cap2.defn"), &format!("include {}/bindings/Configuration/Capabilities/make.FOO.defn\n", config.display()));
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
        // `$(shell)` in the configuration's shell; `USESTHORNS` as the
        // build passes it.
        thorn("Bash", "SRCS = a.c\nCFLAGS += $(shell [[ -n yes ]] && echo -fprofile-exclude-files=x)\n", None);
        thorn("Uses", "SRCS = a.c\nifneq ($(filter Foo,$(USESTHORNS)),)\nCFLAGS += -fprofile-filter-files=x\nendif\n", None);
        // A file make passes over and the fragment stands down for.
        thorn("Unread", "SRCS = a.c\n", Some("CFLAGS += -O1\n"));
        let unread = root.join("arrangements/Arr/Unread/src/make.code.deps");
        std::fs::set_permissions(&unread, std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();
        let can_unread = std::fs::File::open(&unread).is_err();
        // Built by a Makefile of its own.
        thorn("Own", "SRCS = a.c\n", None);
        write(root.join("arrangements/Arr/Own/src/Makefile"), "all:\n");
        // A link to nothing: no file, for make and the fragment alike.
        thorn("Dangle", "SRCS = a.c\n", None);
        std::os::unix::fs::symlink(root.join("nowhere.deps"), root.join("arrangements/Arr/Dangle/src/make.code.deps")).unwrap();
        // Two flags of one variable the cache refuses: both said.
        thorn("Two", "SRCS = a.c\nCFLAGS += --coverage -pg\n", None);
        // A flag the cache does not follow, with what may be its value:
        // that word is no flag of its own.
        thorn("Mllvm", "SRCS = a.c\nCFLAGS += -mllvm -inline-threshold=500\n", None);
        // No source of its own; its bindings, compiled as a directory of
        // its, get the capability's flag.
        thorn("Bnd", "SRCS =\n", None);
        write(config.join("bindings/Configuration/Thorns/make.Bnd.defn"), &format!("include {}/bindings/Configuration/Capabilities/make.FOO.defn\n", config.display()));
        write(config.join("bindings/build/Bnd/make.code.defn"), "SRCS = cctk_ThornBindings.c\n");
        write(config.join("bindings/build/Bnd/cctk_ThornBindings.c"), "int b(void) { return 0; }\n");
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
        assert!(about("-fauto-profile").is_some_and(|thorn| thorn.starts_with("Arr/Cap, Arr/Cap2, Arr/Bnd (") && thorn.contains("make.FOO.defn")), "{found:?}");
        assert_eq!(kept.iter().filter(|raw| raw.reason.contains("-fauto-profile")).count(), 1, "{found:?}");
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
        assert!(about("-fprofile-exclude-files=x").is_some_and(|thorn| thorn.starts_with("Arr/Bash")), "{found:?}");
        assert!(about("-fprofile-filter-files=x").is_some_and(|thorn| thorn.starts_with("Arr/Uses")), "{found:?}");
        if can_unread {
            assert!(kept.iter().any(|raw| raw.thorn.as_deref().is_some_and(|thorn| thorn.starts_with("Arr/Unread")) && raw.reason.contains("cannot be read")), "{found:?}");
        }
        std::fs::set_permissions(&unread, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
        assert!(of("Arr/Dangle").is_empty(), "{found:?}");
        let two: Vec<&&Raw> = kept.iter().filter(|raw| raw.thorn.as_deref().is_some_and(|thorn| thorn.starts_with("Arr/Two "))).collect();
        // The first only, and a word that the rest is not checked.
        assert!(two.len() == 1 && two[0].reason.contains("--coverage"), "{found:?}");
        assert!(found.iter().any(|raw| raw.severity == Severity::Unchecked && raw.reason == FIRST_ONLY), "{found:?}");
        let mllvm: Vec<&&Raw> = kept.iter().filter(|raw| raw.thorn.as_deref().is_some_and(|thorn| thorn.starts_with("Arr/Mllvm "))).collect();
        assert!(mllvm.len() == 1 && mllvm[0].reason.starts_with("-mllvm"), "{found:?}");
        let unchecked: Vec<&Raw> = found.iter().filter(|raw| raw.severity == Severity::Unchecked && raw.reason != FIRST_ONLY).collect();
        assert!(unchecked.len() == 1 && unchecked[0].thorn.as_deref().is_some_and(|thorn| thorn.starts_with("Arr/Own")), "{found:?}");
    }

    /// The reader's first reason, with the word it is about where it names
    /// none: the word taken for a second input (not a value the reader
    /// takes apart from its flag), the word read as a second `-o`; nvcc's
    /// reasons about the compile as a whole are of all its words together.
    #[test]
    fn says_the_first_refused_flag_and_the_word_it_is_about() {
        let said = |family, flags: &str, source: &str| first_reason(|words: &[OsString], with_source: bool| parse_flags(family, words, with_source.then_some(source)), &flags.split_whitespace().map(OsString::from).collect::<Vec<_>>()).map(|(why, _)| why);
        let gcc = |flags| said(identity::Family::Gcc, flags, "probe.c");
        assert_eq!(gcc("-O2 --coverage -pg").as_deref(), Some("--coverage is a flag the cache does not follow"));
        assert_eq!(gcc("-mllvm -inline-threshold=500 -O2").as_deref(), Some("-mllvm is a flag the cache does not follow"));
        assert_eq!(gcc("-O2"), None);
        assert_eq!(gcc("-O2 -Wp,-D_FORTIFY_SOURCE=3").as_deref().and_then(reason_words), Some(vec!["-Wp,-D_FORTIFY_SOURCE=3"]));
        // The stray word, after a value the reader takes apart from its
        // flag.
        for (flags, stray) in [("-O2 -I /usr/include /opt/x.a", "/opt/x.a"), ("-D NDEBUG CCTK_DEBUG", "CCTK_DEBUG"), ("-include stdio.h extra.o", "extra.o"), ("-isystem /usr/include /usr/local/include", "/usr/local/include"), ("-O2 -L /opt/lib", "/opt/lib")] {
            assert_eq!(gcc(flags), Some(format!("there is more than one input file ({stray} is read as one)")), "{flags}");
        }
        // The position of the word, for its variable.
        let at = first_reason(|words: &[OsString], with_source: bool| parse_flags(identity::Family::Gcc, words, with_source.then_some("probe.c")), &["-DFOO", "-isystem", "/usr/local/include", "-g", "/usr/local/include"].map(OsString::from)).and_then(|(_, at)| at);
        assert_eq!(at, Some(4));
        // A word the reader reads -o out of.
        let why = gcc("-O2 -openmp -g").unwrap();
        assert_eq!(why, "-o is given more than once (-openmp is read as -o and what follows it)");
        assert_eq!(reason_subject(&why), Some("-openmp"));
        assert_eq!(reason_subject("-pg is not a flag the cache knows"), Some("-pg"));
        assert_eq!(reason_subject("nvcc compiles without --objdir-as-tempdir are not the same twice"), None);
        let nvcc = |flags| said(identity::Family::Nvcc, flags, "probe.cu");
        assert!(nvcc("-Xcompiler -pg --objdir-as-tempdir -x cu").is_some_and(|why| why.starts_with("-pg")));
        assert_eq!(nvcc("--objdir-as-tempdir -O3"), None);
        assert_eq!(nvcc("--objdir-as-tempdir --gpu-architecture sm_80 sm_86").as_deref(), Some("there is more than one input file (sm_86 is read as one)"));
    }

    /// The checks get the setup's TMPDIR (the build's), set, empty or not
    /// set, a read-only one too, and make their files in their own; one
    /// where no file can be made keeps every compile out.
    #[test]
    fn the_checks_get_the_build_s_tmpdir() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("cactup");
        // Says what it is given: its TMPDIR as exported, and its arguments.
        std::fs::write(&fake, "#!/bin/sh\nprintf '%s|%s|%s' \"${TMPDIR-unset}\" \"$1\" \"$2\"\n").unwrap();
        super::super::make_executable(&fake);
        let work = tmp.path().join("work");
        let run = |env: &str| {
            let out = Command::new("/bin/sh").arg("-c").arg(setup_then(env, &checks_command(&work, &fake))).env_remove("TMPDIR").output().unwrap();
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        let work = work.display();
        assert_eq!(run(""), format!("unset|{DOCTOR_VERB}|{work}"));
        assert_eq!(run("export TMPDIR=/nowhere/x"), format!("/nowhere/x|{DOCTOR_VERB}|{work}"));
        assert_eq!(run("export TMPDIR=/nowhere/x\nreadonly TMPDIR"), format!("/nowhere/x|{DOCTOR_VERB}|{work}"));
        // Not exported: not the build's either.
        assert_eq!(run("TMPDIR=/nowhere/x"), format!("unset|{DOCTOR_VERB}|{work}"));
        assert!(unusable_tmpdir(Some("/nowhere/x".into()), None).is_some_and(|raw| raw.severity == Severity::KeepsOut));
        assert!(unusable_tmpdir(Some(tmp.path().to_owned()), None).is_none());
        // Relative: where the compiles run, if there is such a place yet.
        let scratch = tmp.path().join("scratch");
        std::fs::create_dir_all(scratch.join("tmp")).unwrap();
        assert!(unusable_tmpdir(Some("~/tmp".into()), None).is_none());
        assert!(unusable_tmpdir(Some("~/tmp".into()), Some(&scratch)).is_some_and(|raw| raw.reason.contains("no absolute path")));
        assert!(unusable_tmpdir(Some("tmp".into()), Some(&scratch)).is_none());
        assert!(unusable_tmpdir(Some("".into()), Some(&scratch)).is_none());
        // A flag's value goes with it in what to leave out.
        let value = "-O2 -mllvm -x86-asm-syntax=intel".to_owned();
        assert_eq!(shown_for("-mllvm is a flag the cache does not follow", Some(&value)).unwrap(), "-mllvm -x86-asm-syntax=intel");
        let value = "-O2 -L /opt/lib -g".to_owned();
        assert_eq!(shown_for("-L is not a flag the cache knows", Some(&value)).unwrap(), "-L /opt/lib");
        let value = "-O2 --coverage /opt/x.a".to_owned();
        assert_eq!(shown_for("--coverage is a flag the cache does not follow", Some(&value)).unwrap(), "--coverage");
        let value = "-O3 -override-limits".to_owned();
        assert_eq!(shown_for("-o is given more than once", Some(&value)).unwrap(), "-override-limits");
        let value = "-O2 -override-limits -openmp".to_owned();
        assert_eq!(shown_for("-o is given more than once (-openmp is read as -o and what follows it)", Some(&value)).unwrap(), "-openmp");
        let value = "-mllvm -a -O2 -mllvm -b".to_owned();
        assert_eq!(shown_for("-mllvm is a flag the cache does not follow", Some(&value)).unwrap(), "-mllvm -a and -mllvm -b");
        assert_eq!(reason_words("--ptxas-options=-O3 -O3 is not a flag the cache knows"), Some(vec!["--ptxas-options=-O3"]));
    }

    /// With the first reason only, a thorn that takes the configuration's
    /// first refused flag out shows its next: that is the configuration's,
    /// not the thorn's. A second input goes to the variable it is in, not
    /// to one that has the same word as a flag's value.
    #[test]
    fn a_reason_is_the_variable_s_and_the_configuration_s_where_it_is() {
        if identity::find_program(OsStr::new("gcc")).is_err() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let c: Vec<Language> = languages().into_iter().filter(|language| language.name == "C").collect();
        let request = Request { config_dir: None, optionlist: BTreeMap::new(), conf: conf(tmp.path()), languages: c.clone() };
        let values = |cflags: &str, cppflags: &str| -> BTreeMap<String, String> { [("CC", "gcc"), ("CPPFLAGS", cppflags), ("CFLAGS", cflags)].map(|(name, value)| (name.to_owned(), value.to_owned())).into() };
        let configuration = values("-O2 -flto=auto -fipa-pta", "");
        let base = check_language(&request, &c[0], &configuration, tmp.path(), false);
        assert!(base.iter().any(|raw| raw.reason.starts_with("-flto=auto")), "{base:?}");
        let thorn = values("-O2 -fipa-pta", "");
        let found = differences(&request, &c[0], &configuration, &thorn, &base, tmp.path(), false, &mut Memo::new());
        assert!(found.is_empty(), "{found:?}");
        let found = check_language(&request, &c[0], &values("-g /usr/local/include", "-DFOO -isystem /usr/local/include"), tmp.path(), false);
        let second: Vec<&Raw> = found.iter().filter(|raw| raw.reason.contains("is read as one")).collect();
        assert!(second.len() == 1 && second[0].variable.as_deref() == Some("CFLAGS"), "{found:?}");
        // A word read as -o: of each variable that has it.
        let found = check_language(&request, &c[0], &values("-g -openmp", "-openmp"), tmp.path(), false);
        let read: Vec<Option<&str>> = found.iter().filter(|raw| raw.reason.contains("is read as -o")).map(|raw| raw.variable.as_deref()).collect();
        assert_eq!(read, [Some("CPPFLAGS"), Some("CFLAGS")], "{found:?}");
        // Where the configuration's check read all its words, a thorn's
        // reason about a word it has (as a value) is the thorn's.
        let configuration = values("-O2", "-isystem /usr/local/include");
        let base = check_language(&request, &c[0], &configuration, tmp.path(), false);
        let found = differences(&request, &c[0], &configuration, &values("-O2 /usr/local/include", "-isystem /usr/local/include"), &base, tmp.path(), false, &mut Memo::new());
        assert!(found.iter().any(|raw| raw.reason.contains("/usr/local/include is read as one")), "{found:?}");
    }

    /// A configure that failed, as Cactus records it.
    #[test]
    fn reads_whether_the_last_configure_failed() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!configure_failed(tmp.path()));
        std::fs::write(tmp.path().join("config-info"), "# CONFIGURATION  : et\n# CONFIG-STATUS  : 0\n# CONFIG-OPTIONS :\nCC=gcc\n").unwrap();
        assert!(!configure_failed(tmp.path()));
        std::fs::write(tmp.path().join("config-info"), "# CONFIGURATION  : et\n# CONFIG-STATUS  : 1\n# CONFIG-OPTIONS :\nCC=gcc\n").unwrap();
        assert!(configure_failed(tmp.path()));
    }

    /// An option list read as Cactus reads it: comments, the first setting
    /// (its variables' names are what the trace looks up).
    #[test]
    fn reads_an_option_list_as_cactus_does() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("ol.cfg");
        std::fs::write(&file, "# a comment\nCC = gcc # not -pg\nCFLAGS -O2\nCFLAGS = -O3\nCXX = ${CACTUP_DOCTOR_TEST_CXX}\nLIBS = a$$b\n").unwrap();
        let values = optionlist_values(&file);
        assert_eq!(values.get("CC").map(String::as_str), Some("gcc"));
        assert_eq!(values.get("CFLAGS").map(String::as_str), Some("-O2"));
        assert_eq!(values.get("LIBS").map(String::as_str), Some("a$$b"));
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
