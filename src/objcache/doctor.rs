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
use std::process::{Command, Stdio};

/// The verb the build environment's shell runs the checks by: `cactup
/// __cc-doctor`, with a [`Request`] on stdin.
pub const DOCTOR_VERB: &str = "__cc-doctor";

/// How much a finding matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Severity {
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
        language("CUDA", "CUCC", &["CUCCFLAGS"], "cu", "__global__ void probe() {}\n"),
    ]
}

/// The environment variables whose entries the cache's rules look at, and
/// that a shell's `export X=/dir:$X` can give an empty entry.
pub const PATH_VARIABLES: [&str; 9] =
    ["PATH", "LD_LIBRARY_PATH", "LIBRARY_PATH", "CPATH", "C_INCLUDE_PATH", "CPLUS_INCLUDE_PATH", "COMPILER_PATH", "GCC_EXEC_PREFIX", "LD_PRELOAD"];

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
    for variable in PATH_VARIABLES.iter().chain(environment::REFUSED.iter()) {
        if let Some(value) = std::env::var_os(variable) {
            reply.env.insert((*variable).to_owned(), value.to_string_lossy().into_owned());
        }
    }
    reply.values = match &request.config_dir {
        Some(config_dir) => made_values(config_dir, &request.languages).unwrap_or_else(|_| request.optionlist.clone()),
        None => request.optionlist.clone(),
    };
    // The environment: what keeps every compile out.
    for reason in environment::refusals(std::env::vars_os()) {
        let variable = reason.split_whitespace().next().map(str::to_owned);
        reply.found.push(Raw { severity: Severity::KeepsOut, language: None, variable, reason });
    }
    // Entries of the path variables that are not absolute: some keep a
    // kind of compile out (below, in the probe's key); every one makes each
    // compile look at more places.
    for variable in PATH_VARIABLES {
        if let Some(value) = std::env::var_os(variable)
            && value.as_encoded_bytes().split(|b| *b == b':').any(|entry| !entry.starts_with(b"/"))
        {
            reply.found.push(Raw {
                severity: Severity::Note,
                language: None,
                variable: Some(variable.to_owned()),
                reason: format!("{variable} has an entry that is not an absolute path (an empty one is the working directory)"),
            });
        }
    }
    let Ok(tmp) = tempfile::tempdir() else { return reply };
    for language in &request.languages {
        reply.found.extend(check_language(request, language, &reply.values, tmp.path(), probe));
    }
    reply
}

/// The compiler and flags of `language`, as `values` has them, checked as a
/// compile of it through the wrapper would be.
fn check_language(request: &Request, language: &Language, values: &BTreeMap<String, String>, tmp: &Path, probe: bool) -> Vec<Raw> {
    let mut found = Vec::new();
    let raw = |severity, variable: Option<&str>, reason: String| Raw { severity, language: Some(language.name.clone()), variable: variable.map(str::to_owned), reason };
    let Some(compiler) = values.get(&language.compiler_variable).filter(|value| !value.trim().is_empty() && value.trim() != "none") else {
        return found;
    };
    let words: Vec<OsString> = compiler.split_whitespace().map(OsString::from).collect();
    let name = words[0].clone();
    // Not there at all: this configuration may not compile the language.
    if identity::find_program(&name).is_err() {
        found.push(raw(Severity::Note, Some(&language.compiler_variable), format!("{} is not on PATH in the build environment", name.to_string_lossy())));
        return found;
    }
    let cc_dir = tmp.join(format!("cc-{}", language.suffix));
    let _ = std::fs::create_dir_all(&cc_dir);
    let fortran = language.name == "Fortran";
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
    let parse = |flags: &[OsString]| {
        let mut args: Vec<OsString> = words[1..].to_vec();
        args.extend(flags.iter().cloned());
        args.extend(["-c", "-o", "probe.o", &source].map(OsString::from));
        match compiler.family {
            identity::Family::Nvcc => compile::parse_cuda(&args).map(|_| ()),
            _ => compile::parse(&args).map(|_| ()),
        }
    };
    let mut every_flag = Vec::new();
    for variable in &language.flag_variables {
        let flags: Vec<OsString> = values.get(variable).map(|value| value.split_whitespace().map(OsString::from).collect()).unwrap_or_default();
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
            if let Some(why) = keyed.unnarrowed {
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
    let defn = config_dir.join("config-data/make.config.defn");
    if !defn.is_file() {
        anyhow::bail!("{} is not there", defn.display());
    }
    let mut variables: Vec<&str> = Vec::new();
    for language in languages {
        variables.push(&language.compiler_variable);
        variables.extend(language.flag_variables.iter().map(String::as_str));
    }
    variables.sort();
    variables.dedup();
    let dir = tempfile::tempdir().context("Failed to make a temporary directory")?;
    let mut makefile = format!("TOP := {}\ninclude {}\nprint:\n", config_dir.display(), defn.display());
    for variable in &variables {
        makefile.push_str(&format!("\t@printf '%s\\n' '{variable}=$({variable})'\n"));
    }
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
/// nothing here. Each finding: the variable, how it was before, its value
/// after.
pub fn fragile_entries(env: &str) -> Vec<(String, &'static str, String)> {
    let mut found = Vec::new();
    for state in ["unset", "empty", "set"] {
        let mut script = String::from("module() { :; }; ml() { :; }\n");
        script.push_str(env);
        script.push('\n');
        for variable in PATH_VARIABLES {
            script.push_str(&format!("printf '%s=%s\\n' {variable} \"${{{variable}-<unset>}}\"\n"));
        }
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(&script).env_clear().env("HOME", "/nonexistent-home").stdin(Stdio::null()).stderr(Stdio::null());
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
        let Ok(out) = command.output() else { continue };
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let Some((variable, value)) = line.split_once('=') else { continue };
            // Left as it came (unset, or empty as it was): not the setup's.
            if value == "<unset>" || (value.is_empty() && state != "set") {
                continue;
            }
            if value.split(':').any(|entry| !entry.starts_with('/')) && !found.iter().any(|(found, _, _): &(String, _, _)| found == variable) {
                found.push((variable.to_owned(), state, value.to_owned()));
            }
        }
    }
    found
}

/// Run the checks of [`Request`] in the build environment `env` (the
/// attempt's frozen setup), from `cactus_root`, by `cactup`.
pub fn in_build_environment(cactup: &Path, cactus_root: &Path, env: &str, request: &Request) -> Res<Reply> {
    let script = format!("{env}\nexec {} {DOCTOR_VERB}", crate::build::sh_quote(cactup));
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(&script)
        .current_dir(cactus_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to start the build environment's shell")?;
    let input = serde_json::to_vec(request).context("Failed to write the checks' request")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&input).context("Failed to hand the build environment what to check")?;
    }
    let out = wait_interruptibly(child)?;
    if !out.status.success() {
        anyhow::bail!(
            "the checks in the build environment did not run ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or_default()
        );
    }
    serde_json::from_slice(&out.stdout).context("The checks in the build environment answered in a form this cactup cannot read")
}

/// Wait for `child`, stopping it on Ctrl-C (`CLAUDE.md`'s interrupt
/// contract): a module system's setup can take a while.
fn wait_interruptibly(mut child: std::process::Child) -> Res<std::process::Output> {
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let reading = std::thread::spawn(move || {
        let mut out = Vec::new();
        if let Some(stdout) = stdout.as_mut() {
            let _ = stdout.read_to_end(&mut out);
        }
        out
    });
    let reading_err = std::thread::spawn(move || {
        let mut err = Vec::new();
        if let Some(stderr) = stderr.as_mut() {
            let _ = stderr.read_to_end(&mut err);
        }
        err
    });
    loop {
        if let Some(status) = child.try_wait().context("Failed to wait for the build environment")? {
            let stdout = reading.join().unwrap_or_default();
            let stderr = reading_err.join().unwrap_or_default();
            return Ok(std::process::Output { status, stdout, stderr });
        }
        if gix::interrupt::is_triggered() {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("interrupted");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
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
            severity: Severity::Note,
            what: format!("this configuration builds in the universe \"{}\", whose environment is not checked here", universe.name),
            from: "the configuration".to_owned(),
            fix: None,
        });
        return Ok(findings);
    }
    let optionlist = optionlist_values(&attempt.optionlist_path());
    let defaults = configured_values(&meta.config_dir);
    let configured = meta.config_dir.join("config-data/make.config.defn").is_file();
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
    // One finding per reason: the languages it was found for (none, when
    // it is about every compile), and the most it matters.
    let mut merged: Vec<(Severity, &Raw, Vec<&str>)> = Vec::new();
    for raw in &reply.found {
        match merged.iter_mut().find(|(_, seen, _)| seen.reason == raw.reason) {
            Some((severity, _, languages)) => {
                *severity = (*severity).min(raw.severity);
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
    for (variable, state, value) in fragile_entries(&meta.build_env) {
        findings.push(Finding {
            severity: Severity::Note,
            what: format!("with {variable} {state} before it runs, the setup makes it \"{value}\", with an entry that is not an absolute path (an empty one is the working directory)"),
            from: setup.clone(),
            fix: Some(format!("write it as export {variable}=\"/dir${{{variable}:+:${variable}}}\"")),
        });
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
                vec![Finding { severity: Severity::Note, what: format!("the build environment could not be checked: {e:#}"), from: "the build environment".to_owned(), fix: None }]
            })
        }),
    }]
}

/// Where a finding comes from, and what to change.
fn trace(raw: &Raw, reply: &Reply, optionlist: &BTreeMap<String, String>, defaults: &BTreeMap<String, String>, source: &str, setup: &str) -> (String, Option<String>) {
    // About an environment variable: the user's shell if it had the
    // variable (with the entries found) before the setup ran, else the
    // setup.
    let variable = raw.variable.clone().or_else(|| raw.reason.split_whitespace().next().filter(|word| word.chars().all(|c| c.is_ascii_uppercase() || c == '_')).map(str::to_owned));
    if let Some(variable) = variable.as_deref().filter(|variable| reply.env.contains_key(*variable) || PATH_VARIABLES.contains(variable) || environment::REFUSED.contains(variable)) {
        let inner = reply.env.get(variable).cloned().unwrap_or_default();
        let outer = std::env::var(variable).ok();
        let not_absolute = |value: &str| value.split(':').filter(|entry| !entry.starts_with('/')).map(str::to_owned).collect::<Vec<_>>();
        let from_shell = match &outer {
            Some(outer) if raw.reason.contains("is set") => !outer.is_empty() || inner.is_empty(),
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
        let fix = match raw.reason.contains("is set") {
            true => format!("unset {variable} for the build"),
            false => format!("give {variable} absolute directories only: export {variable}=\"/dir${{{variable}:+:${variable}}}\" adds one without an empty entry"),
        };
        return (from, Some(fix));
    }
    // About a flag: the option list's variable that has it, else Cactus's
    // defaults for the compiler (what configure wrote).
    if let Some(flag) = raw.reason.split([' ', ',']).next().filter(|word| word.starts_with('-')) {
        let has = |values: &BTreeMap<String, String>| values.iter().find(|(_, value)| value.split_whitespace().any(|word| word == flag || word.starts_with(&format!("{flag}=")))).map(|(name, _)| name.clone());
        let from = match (has(optionlist), has(defaults)) {
            (Some(name), _) => format!("{source}, {name}"),
            (None, Some(name)) => format!("Cactus's default {name} for this compiler (configure wrote it to config-data/make.config.defn; the option list can set {name} itself)"),
            (None, None) => format!("{source} or Cactus's defaults, {}", raw.variable.as_deref().unwrap_or("the flags")),
        };
        let fix = hint(&raw.reason).map(str::to_owned).or_else(|| Some(format!("leave {flag} out, if the build does not need it")));
        return (from, fix);
    }
    // About the compiler.
    if let Some(variable) = raw.variable.as_deref() {
        let value = reply.values.get(variable).cloned().unwrap_or_default();
        return (format!("{source}, {variable} = {value}"), hint(&raw.reason).map(str::to_owned));
    }
    ("the build environment".to_owned(), hint(&raw.reason).map(str::to_owned))
}

/// What would let compiles kept out for `why` in, where cactup knows.
pub fn hint(why: &str) -> Option<&'static str> {
    let hints: [(&str, &str); 9] = [
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
        let c: Vec<Language> = languages().into_iter().filter(|language| language.name == "C" || language.name == "CUDA").collect();
        let optionlist: BTreeMap<String, String> = [
            ("CC", "gcc"),
            ("CPPFLAGS", "-DX=1"),
            ("CFLAGS", "-g -O2 -fprofile-generate"),
            ("CUCC", "no-such-nvcc-here"),
        ]
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .into();
        let request = Request { config_dir: None, optionlist: optionlist.clone(), conf: conf(tmp.path()), languages: c };
        let reply = run(&request, false);
        let refused: Vec<&Raw> = reply.found.iter().filter(|raw| raw.severity == Severity::KeepsOut && raw.language.as_deref() == Some("C")).collect();
        assert!(refused.iter().any(|raw| raw.variable.as_deref() == Some("CFLAGS") && raw.reason.starts_with("-fprofile-generate")), "{:?}", reply.found);
        assert!(!refused.iter().any(|raw| raw.variable.as_deref() == Some("CPPFLAGS")), "{:?}", reply.found);
        assert!(reply.found.iter().any(|raw| raw.language.as_deref() == Some("CUDA") && raw.reason.contains("not on PATH")), "{:?}", reply.found);
        let raw = refused.iter().find(|raw| raw.variable.as_deref() == Some("CFLAGS")).unwrap();
        let defaults: BTreeMap<String, String> = [("C_OPTIMISE_FLAGS".to_owned(), "-O3".to_owned())].into();
        let (from, fix) = trace(raw, &reply, &optionlist, &defaults, "the option list (x)", "the setup");
        assert_eq!(from, "the option list (x), CFLAGS");
        assert!(fix.is_some());
        // A flag configure added: Cactus's defaults.
        let from_defaults = Raw { reason: "-O3 is a flag the cache does not follow".into(), ..(*raw).clone() };
        assert!(trace(&from_defaults, &reply, &BTreeMap::new(), &defaults, "x", "y").0.starts_with("Cactus's default C_OPTIMISE_FLAGS"));
    }

    /// A setup that gives a variable an empty entry where it was not set
    /// before is found; the idiom that does not, is not.
    #[test]
    fn finds_a_setup_that_gives_an_empty_entry() {
        let found = fragile_entries("export LD_LIBRARY_PATH=/opt/mpi/lib:$LD_LIBRARY_PATH\nexport CPATH=\"${CPATH:+$CPATH:}/opt/inc\"\nmodule load gcc\n");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!((found[0].0.as_str(), found[0].1, found[0].2.as_str()), ("LD_LIBRARY_PATH", "unset", "/opt/mpi/lib:"));
        assert!(fragile_entries("export PATH=/opt/bin:$PATH\n").is_empty(), "PATH is always set");
        assert_eq!(fragile_entries("export LIBRARY_PATH=lib\n")[0].0, "LIBRARY_PATH");
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
                for (variable, state, value) in fragile_entries(&env) {
                    bad.push(format!("{name} (universe {universe:?}): with {variable} {state} before, it is \"{value}\""));
                }
            }
        }
        assert!(bad.is_empty(), "build environment setups that give a path variable an entry that is not an absolute path (write `export X=\"/dir${{X:+:$X}}\"`):\n{}", bad.join("\n"));
        assert!(checked > 20, "only {checked} setups checked");
    }
}
