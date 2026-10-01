//! The build cache's compiler wrapper and probe, driven as `make` and the
//! build script drive them: through the cactup binary.

use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const CACTUP: &str = env!("CARGO_BIN_EXE_cactup");

/// A build attempt's `cc` directory with a frozen configuration in `mode`,
/// inside a tree shaped like a Cactus installation.
struct Build {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    config: PathBuf,
    cc: PathBuf,
}

impl Build {
    fn new(mode: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("Cactus");
        let config = root.join("configs/sim");
        let cc = config.join(".cactup-builds/0000/cc");
        fs::create_dir_all(&cc).unwrap();
        fs::create_dir_all(config.join("config-data")).unwrap();
        fs::create_dir_all(root.join("lib/make")).unwrap();
        fs::write(
            cc.join("config.toml"),
            format!(
                "version = 1\nmode = \"{mode}\"\ncactup = \"{CACTUP}\"\ncache-root = \"{}\"\n\
                 cactus-root = \"{}\"\nconfig-dir = \"{}\"\nmachine = \"test\"\nbuild-env-digest = \"\"\n",
                tmp.path().join("cache").display(),
                root.display(),
                config.display(),
            ),
        )
        .unwrap();
        Self { _tmp: tmp, root, config, cc }
    }

    fn conf(&self) -> PathBuf {
        self.cc.join("config.toml")
    }

    /// The logged compiles, one JSON object per line.
    fn events(&self) -> Vec<String> {
        match fs::read_to_string(self.cc.join("events.jsonl")) {
            Ok(text) => text.lines().map(str::to_owned).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// `cactup __cc <conf> <args…>`.
    fn wrap(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(CACTUP);
        cmd.arg("__cc").arg(self.conf()).args(args);
        cmd.env_remove("CACTUP_CC_TEST_PANIC").env_remove("CACTUP_CC_DEBUG");
        cmd
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn assert_ran(out: &Output, stdout: &str, stderr: &str, code: i32) {
    assert_eq!(
        (text(&out.stdout).as_str(), text(&out.stderr).as_str(), out.status.code()),
        (stdout, stderr, Some(code))
    );
}

/// An executable script.
fn script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

const COMPILE: &str = "echo out; echo err >&2; exit 3";

#[test]
fn without_a_readable_configuration_it_is_just_the_compiler() {
    let out = Command::new(CACTUP)
        .args(["__cc", "/nonexistent/cc/config.toml", "sh", "-c", COMPILE])
        .output()
        .unwrap();
    assert_ran(&out, "out\n", "err\n", 3);

    // A configuration of another version is as good as none.
    let build = Build::new("record");
    fs::write(build.conf(), "version = 999\n").unwrap();
    assert_ran(&build.wrap(&["sh", "-c", COMPILE]).output().unwrap(), "out\n", "err\n", 3);
    assert!(build.events().is_empty());
}

#[test]
fn off_runs_the_compiler_and_logs_nothing() {
    let build = Build::new("off");
    assert_ran(&build.wrap(&["sh", "-c", COMPILE]).output().unwrap(), "out\n", "err\n", 3);
    assert!(build.events().is_empty());
}

#[test]
fn record_adds_nothing_to_the_output_and_logs_the_compile() {
    let build = Build::new("record");
    assert_ran(&build.wrap(&["sh", "-c", COMPILE]).output().unwrap(), "out\n", "err\n", 3);
    let events = build.events();
    assert_eq!(events.len(), 1, "{events:?}");
    assert!(events[0].contains("\"compiler\":\"sh\"") && events[0].contains("\"exit\":3"), "{}", events[0]);
}

#[test]
fn stdin_reaches_the_compiler() {
    let build = Build::new("record");
    let mut child = build.wrap(&["cat"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    {
        use std::io::Write;
        child.stdin.take().unwrap().write_all(b"int main;\n").unwrap();
    }
    let out = child.wait_with_output().unwrap();
    assert_eq!(text(&out.stdout), "int main;\n");
}

#[test]
fn the_marked_form_runs_the_configured_command() {
    let build = Build::new("record");
    // The variable is not in the environment at all: the words stand.
    let out = build.wrap(&["CC:3", "sh", "-c", "echo \"$0-$1\"", "name", "arg"]).env_remove("CC").output().unwrap();
    assert_ran(&out, "name-arg\n", "", 0);
    // It is, and says the same.
    let out = build.wrap(&["CC:1", "echo", "compiled"]).env("CC", " echo ").output().unwrap();
    assert_ran(&out, "compiled\n", "", 0);
    assert_eq!(build.events().len(), 2);
}

#[test]
fn a_reassigned_compiler_variable_wins_and_is_not_logged() {
    let build = Build::new("record");
    // The makefiles set CC to something else after the fragment was read:
    // that command runs, through the shell, with the compile's arguments.
    let out = build
        .wrap(&["CC:1", "false", "-c", "a b.c"])
        .env("CC", "printf '%s|' thorn \"own cc\"")
        .output()
        .unwrap();
    assert_ran(&out, "thorn|own cc|-c|a b.c|", "", 0);
    assert!(build.events().is_empty());
}

#[test]
fn it_dies_of_the_signal_that_stopped_the_compiler() {
    let build = Build::new("record");
    let status = build.wrap(&["sh", "-c", "kill -TERM $$"]).status().unwrap();
    assert_eq!(status.signal(), Some(15), "{status:?}");
    assert!(build.events()[0].contains("\"signal\":15"));
    // A crashed compiler is an exit code, not a core file of cactup.
    let status = build.wrap(&["sh", "-c", "kill -SEGV $$"]).status().unwrap();
    assert_eq!(status.code(), Some(128 + 11), "{status:?}");
}

#[test]
fn a_signal_to_the_wrapper_reaches_the_compiler() {
    let build = Build::new("record");
    let mut child = build
        .wrap(&["sh", "-c", "trap 'exit 42' TERM; echo ready; sleep 20 & wait"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut ready).unwrap();
    assert_eq!(ready, "ready\n");
    assert!(Command::new("kill").args(["-TERM", &child.id().to_string()]).status().unwrap().success());
    // The compiler's own answer to the signal is the wrapper's exit status.
    assert_eq!(child.wait().unwrap().code(), Some(42));
}

#[test]
fn by_name_it_takes_the_configuration_from_the_environment() {
    let build = Build::new("record");
    let link = build.root.join("cactup-cc");
    std::os::unix::fs::symlink(CACTUP, &link).unwrap();
    let out = Command::new(&link)
        .args(["sh", "-c", COMPILE])
        .env("CACTUP_CC_CONF", build.conf())
        .output()
        .unwrap();
    assert_ran(&out, "out\n", "err\n", 3);
    assert_eq!(build.events().len(), 1);
    // Without the variable it is still the compiler.
    let out = Command::new(&link).args(["sh", "-c", COMPILE]).env_remove("CACTUP_CC_CONF").output().unwrap();
    assert_ran(&out, "out\n", "err\n", 3);
}

#[test]
fn a_compiler_that_cannot_start_fails_as_it_would_in_the_shell() {
    for mode in ["off", "record"] {
        let build = Build::new(mode);
        let out = build.wrap(&["/nonexistent/bin/gcc", "-c", "a.c"]).output().unwrap();
        assert_eq!(out.status.code(), Some(127), "{mode}");
        assert!(text(&out.stderr).starts_with("cactup: /nonexistent/bin/gcc: "), "{mode}: {}", text(&out.stderr));
    }
    let build = Build::new("record");
    let out = build.wrap(&[]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn a_panic_never_costs_the_compile() {
    let build = Build::new("record");
    // Before the compiler has run: this process becomes the compiler.
    let out = build.wrap(&["sh", "-c", COMPILE]).env("CACTUP_CC_TEST_PANIC", "before").output().unwrap();
    assert_ran(&out, "out\n", "err\n", 3);
    // After: the compiler's exit status stands.
    let out = build.wrap(&["sh", "-c", COMPILE]).env("CACTUP_CC_TEST_PANIC", "after").output().unwrap();
    assert_ran(&out, "out\n", "err\n", 3);
}

#[test]
fn an_ordinary_command_is_untouched() {
    let out = Command::new(CACTUP).arg("--version").output().unwrap();
    assert!(out.status.success());
    assert!(text(&out.stdout).starts_with("cactup "), "{}", text(&out.stdout));
}

/// `make` here, if there is one new enough to matter to the test.
fn make() -> Option<&'static str> {
    Command::new("make").arg("--version").output().ok().filter(|o| o.status.success()).map(|_| "make")
}

/// A fake compiler that records its command line in `log` and, like a
/// compiler, writes the file named after `-o`.
fn fake_compiler(path: &Path, log: &Path, name: &str) {
    script(
        path,
        &format!(
            "echo \"{name} $*\" >> '{}'\nwhile [ $# -gt 0 ]; do [ \"$1\" = -o ] && : > \"$2\"; shift; done\n",
            log.display()
        ),
    );
}

/// A miniature of Cactus's sub-make: object rules that run `$(CC)`/`$(CXX)`
/// from a scratch directory, a dependency-generation rule that runs `$(CC)`
/// too, and a prerequisite build script that reads `CC` from its environment
/// the way an ExternalLibraries `build.sh` does.
const MAKE_SUBDIR: &str = "\
include $(CONFIG)/make.config.defn
-include $(THORN_DEFN)
OBJS = a.c.o b.cc.o
all: $(OBJS) a.c.d
$(OBJS): external-done
external-done:
\t@echo \"external $$CC\" >> $(LOG)
%.c.o:
\t@$(CC) -c -o $@ $*.c
%.cc.o:
\t@$(CXX) -c -o $@ $*.cc
%.c.d:
\t@$(CC) -E -M $*.c
";

#[test]
fn under_make_only_the_object_rules_go_through_the_wrapper() {
    let Some(make) = make() else {
        eprintln!("skipped: no make on this host");
        return;
    };
    let build = Build::new("record");
    let log = build.root.join("log");
    let (cc, cxx, thorn_cc) = (build.root.join("fake-cc"), build.root.join("fake-cxx"), build.root.join("thorn-cc"));
    fake_compiler(&cc, &log, "cc");
    fake_compiler(&cxx, &log, "cxx");
    fake_compiler(&thorn_cc, &log, "thorn-cc");
    let config_data = build.config.join("config-data");
    fs::write(
        config_data.join("make.config.defn"),
        format!("export CC = {}\nexport CXX = {} -std=c++17\nexport F90 = none\n", cc.display(), cxx.display()),
    )
    .unwrap();
    fs::write(config_data.join("make.config.rules"), "%.c.o: $(SRCDIR)/%.c\n%.cc.o: $(SRCDIR)/%.cc\n").unwrap();
    let subdir = build.root.join("lib/make/make.subdir");
    fs::write(&subdir, MAKE_SUBDIR).unwrap();

    // The probe accepts this tree and writes the fragment and its self-test.
    let probe = Command::new(CACTUP).arg("__cc-probe").arg(build.conf()).output().unwrap();
    assert_ran(&probe, "", "", 0);
    let inject = build.cc.join("inject.mk");
    for selftest in ["selftest/lib/make/make.subdir", "selftest/other.mk"] {
        let status = Command::new(make)
            .arg("-s")
            .arg("-f")
            .arg(build.cc.join(selftest))
            .env("MAKEFILES", &inject)
            .status()
            .unwrap();
        assert!(status.success(), "{selftest} fails under this make");
    }

    let run = |dir: &str, thorn_defn: Option<&Path>| {
        let scratch = build.root.join(dir);
        fs::create_dir_all(&scratch).unwrap();
        let mut cmd = Command::new(make);
        cmd.current_dir(&scratch)
            .args(["-s", "-f"])
            .arg(&subdir)
            .arg(format!("CONFIG={}", config_data.display()))
            .arg(format!("LOG={}", log.display()))
            .env("MAKEFILES", &inject)
            .env_remove("CC")
            .env_remove("CXX");
        if let Some(defn) = thorn_defn {
            cmd.arg(format!("THORN_DEFN={}", defn.display()));
        }
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{}{}", text(&out.stdout), text(&out.stderr));
        let lines: Vec<String> = fs::read_to_string(&log).unwrap().lines().map(str::to_owned).collect();
        fs::remove_file(&log).unwrap();
        (scratch, lines)
    };

    // The compilers ran exactly as make asked, whoever stood in front.
    let (scratch, mut lines) = run("scratch", None);
    lines.sort();
    assert_eq!(
        lines,
        [
            "cc -E -M a.c".to_owned(),
            "cc -c -o a.c.o a.c".to_owned(),
            "cxx -std=c++17 -c -o b.cc.o b.cc".to_owned(),
            format!("external {}", cc.display()),
        ]
    );
    assert!(scratch.join("a.c.o").is_file() && scratch.join("b.cc.o").is_file());
    // The two object compiles went through the wrapper; the dependency run
    // and the prerequisite's build script did not.
    let events = build.events();
    assert_eq!(events.len(), 2, "{events:?}");
    assert!(events.iter().any(|e| e.contains("fake-cc\"")) && events.iter().any(|e| e.contains("fake-cxx\"")));

    // A thorn that sets its own CC gets its own CC, and the cache stays out.
    let thorn_defn = build.root.join("make.code.defn");
    fs::write(&thorn_defn, format!("CC = {}\n", thorn_cc.display())).unwrap();
    let (_, mut lines) = run("scratch-thorn", Some(&thorn_defn));
    lines.sort();
    assert_eq!(
        lines,
        [
            "cxx -std=c++17 -c -o b.cc.o b.cc".to_owned(),
            format!("external {}", thorn_cc.display()),
            "thorn-cc -E -M a.c".to_owned(),
            "thorn-cc -c -o a.c.o a.c".to_owned(),
        ]
    );
    let events = build.events();
    assert_eq!(events.len(), 3, "only the C++ compile is logged the second time: {events:?}");

    // Read by a makefile that is not Cactus's make.subdir, the fragment
    // changes nothing: a foreign build with a `.c.o` object is left alone.
    let foreign = build.root.join("foreign.mk");
    fs::write(&foreign, MAKE_SUBDIR).unwrap();
    let out = Command::new(make)
        .current_dir(&scratch)
        .args(["-s", "-B", "-f"])
        .arg(&foreign)
        .arg(format!("CONFIG={}", config_data.display()))
        .arg(format!("LOG={}", log.display()))
        .env("MAKEFILES", &inject)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(build.events().len(), 3, "a foreign makefile's compiles are not wrapped");
}

#[test]
fn the_probe_declines_with_one_line_and_its_own_status() {
    // No make.config.defn: not a configured Cactus configuration.
    let build = Build::new("record");
    fs::write(build.root.join("lib/make/make.subdir"), "").unwrap();
    let out = Command::new(CACTUP).arg("__cc-probe").arg(build.conf()).output().unwrap();
    assert_eq!(out.status.code(), Some(3));
    let stderr = text(&out.stderr);
    assert!(stderr.starts_with("cactup: build cache off for this build: "), "{stderr}");
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(!build.cc.join("inject.mk").exists());

    let out = Command::new(CACTUP).arg("__cc-probe").output().unwrap();
    assert_eq!(out.status.code(), Some(3));
}
