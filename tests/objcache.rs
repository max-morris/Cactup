//! The build cache's compiler wrapper and probe, driven as `make` and the
//! build script drive them: through the cactup binary.

use std::fs;
use std::io::{BufRead, BufReader, Write};
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
        Self::under("", mode)
    }

    /// With the whole tree below a directory named `parent`.
    fn under(parent: &str, mode: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        // make compares physical paths; so does the probe.
        let root = fs::canonicalize(tmp.path()).unwrap().join(parent).join("Cactus");
        let config = root.join("configs/sim");
        let cc = config.join(".cactup-builds/0000/cc");
        for dir in [&cc, &config.join("config-data"), &config.join("build/Thorn"), &config.join("scratch")] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(
            cc.join("config.toml"),
            format!("mode = \"{mode}\"\ncactup = \"{CACTUP}\"\nconfig-dir = \"{}\"\n", config.display()),
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

    /// `CACTUP_CC_CMD=<compiler> cactup __cc <conf> <args…>`, the way an
    /// injected recipe runs it.
    fn wrap(&self, compiler: &str, args: &[&str]) -> Command {
        let mut cmd = Command::new(CACTUP);
        cmd.arg("__cc").arg(self.conf()).args(args);
        cmd.env("CACTUP_CC_CMD", compiler).env("CACTUP_CC_SHELL", "/bin/sh");
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

fn executable(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

const COMPILE: &str = "echo out; echo err >&2; exit 3";

#[test]
fn without_a_readable_configuration_it_is_just_the_compiler() {
    let out = Command::new(CACTUP)
        .args(["__cc", "/nonexistent/cc/config.toml", "-c", COMPILE])
        .env("CACTUP_CC_CMD", "sh")
        .output()
        .unwrap();
    assert_ran(&out, "out\n", "err\n", 3);

    let build = Build::new("record");
    fs::write(build.conf(), "not = \"a configuration\"\n").unwrap();
    assert_ran(&build.wrap("sh", &["-c", COMPILE]).output().unwrap(), "out\n", "err\n", 3);
    assert!(build.events().is_empty());
}

#[test]
fn off_runs_the_compiler_and_logs_nothing() {
    let build = Build::new("off");
    assert_ran(&build.wrap("sh", &["-c", COMPILE]).output().unwrap(), "out\n", "err\n", 3);
    assert!(build.events().is_empty());
}

#[test]
fn record_adds_nothing_to_the_output_and_logs_the_compile() {
    let build = Build::new("record");
    assert_ran(&build.wrap("sh", &["-c", COMPILE]).output().unwrap(), "out\n", "err\n", 3);
    let events = build.events();
    assert_eq!(events.len(), 1, "{events:?}");
    assert!(events[0].contains("\"compiler\":\"sh\"") && events[0].contains("\"exit\":3"), "{}", events[0]);
}

#[test]
fn the_compiler_gets_the_recipes_stdin_and_environment() {
    for mode in ["off", "record"] {
        let build = Build::new(mode);
        let mut child = build.wrap("cat", &[]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(b"int main;\n").unwrap();
        assert_eq!(text(&child.wait_with_output().unwrap().stdout), "int main;\n", "{mode}");

        // The two variables the fragment adds for the wrapper stop at it.
        let show = "echo \"[$CACTUP_CC_CMD][$CACTUP_CC_SHELL][$KEPT]\"";
        let out = build.wrap("sh", &["-c", show]).env("KEPT", "yes").output().unwrap();
        assert_ran(&out, "[][][yes]\n", "", 0);
    }
}

#[test]
fn a_plain_compiler_of_several_words_runs_as_those_words() {
    let build = Build::new("record");
    let out = build.wrap(" sh\t-c ", &["echo \"$0-$1\"", "name", "an arg"]).output().unwrap();
    assert_ran(&out, "name-an arg\n", "", 0);
    assert_eq!(build.events().len(), 1);
}

#[test]
fn a_compiler_that_needs_a_shell_gets_makes_shell_and_is_not_logged() {
    let build = Build::new("record");
    // Quotes: the text means what it meant in the recipe.
    let out = build.wrap("printf '%s|' thorn \"own cc\"", &["-c", "a b.c"]).output().unwrap();
    assert_ran(&out, "thorn|own cc|-c|a b.c|", "", 0);
    // An assignment in front of the command.
    let out = build.wrap("GREETING=hello sh", &["-c", "echo $GREETING"]).output().unwrap();
    assert_ran(&out, "hello\n", "", 0);
    // The shell is the one make runs the recipe with.
    let shell = build.root.join("recipe-shell");
    executable(&shell, "#!/bin/sh\necho \"recipe shell got: $*\"\n");
    let out = build.wrap("FOO=1 gcc", &["-c", "a.c"]).env("CACTUP_CC_SHELL", &shell).output().unwrap();
    assert_ran(&out, &format!("recipe shell got: -c FOO=1 gcc \"$@\" {} -c a.c\n", shell.display()), "", 0);
    assert!(build.events().is_empty());
}

#[test]
fn a_compiler_already_behind_another_wrapper_is_left_to_it() {
    let build = Build::new("record");
    let ccache = build.root.join("ccache");
    executable(&ccache, "#!/bin/sh\necho \"ccache ran: $*\"\n");
    let out = build.wrap(&format!("{} gcc", ccache.display()), &["-c", "a.c"]).output().unwrap();
    assert_ran(&out, "ccache ran: gcc -c a.c\n", "", 0);
    assert!(build.events().is_empty());
}

#[test]
fn a_script_without_an_interpreter_line_still_runs() {
    for mode in ["off", "record"] {
        let build = Build::new(mode);
        let bin = build.root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        executable(&bin.join("sitecc"), "echo \"site compiler: $*\"\n");
        // Named by path, and found through PATH.
        let by_path = bin.join("sitecc").display().to_string();
        assert_ran(&build.wrap(&by_path, &["-c", "a.c"]).output().unwrap(), "site compiler: -c a.c\n", "", 0);
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
        let out = build.wrap("sitecc -O2", &["-c", "a.c"]).env("PATH", path).output().unwrap();
        assert_ran(&out, "site compiler: -O2 -c a.c\n", "", 0);
    }
}

#[test]
fn it_dies_of_the_signal_that_stopped_the_compiler() {
    let build = Build::new("record");
    let status = build.wrap("sh", &["-c", "kill -TERM $$"]).status().unwrap();
    assert_eq!(status.signal(), Some(15), "{status:?}");
    assert!(build.events()[0].contains("\"signal\":15"));
    // SIGQUIT and a crashed compiler are exit codes, not core files of cactup.
    let status = build.wrap("sh", &["-c", "kill -QUIT $$"]).status().unwrap();
    assert_eq!(status.code(), Some(128 + 3), "{status:?}");
    let status = build.wrap("sh", &["-c", "kill -SEGV $$"]).status().unwrap();
    assert_eq!(status.code(), Some(128 + 11), "{status:?}");
}

#[test]
fn a_signal_to_the_wrapper_reaches_the_compiler() {
    let build = Build::new("record");
    let mut child = build
        .wrap("sh", &["-c", "trap 'exit 42' TERM; echo ready; sleep 20 & wait"])
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
fn a_signal_the_build_ignores_stays_ignored() {
    // What `nohup` does to a build: SIGHUP ignored on the way in. The
    // compiler must see it ignored too, and survive it.
    let mask = "grep SigIgn /proc/self/status";
    let ignoring = |compile: &str, build: &Build| {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "trap '' HUP; exec \"$@\"", "sh", CACTUP, "__cc"]).arg(build.conf()).args(["-c", compile]);
        cmd.env("CACTUP_CC_CMD", "sh").output().unwrap()
    };
    let bare = Command::new("sh").args(["-c", &format!("trap '' HUP; exec sh -c '{mask}'")]).output().unwrap();
    for mode in ["off", "record"] {
        let build = Build::new(mode);
        assert_eq!(text(&ignoring(mask, &build).stdout), text(&bare.stdout), "{mode}");
        assert_ran(&ignoring("kill -HUP $$; echo survived", &build), "survived\n", "", 0);
    }
    // In record mode the compiler's parent is the wrapper: it survives too.
    let build = Build::new("record");
    assert_ran(&ignoring("kill -HUP $PPID; sleep 0.1; echo survived", &build), "survived\n", "", 0);
    assert!(build.events()[0].contains("\"exit\":0"));
}

#[test]
fn a_compiler_that_cannot_start_fails_as_it_would_in_the_shell() {
    for mode in ["off", "record"] {
        let build = Build::new(mode);
        let out = build.wrap("/nonexistent/bin/gcc", &["-c", "a.c"]).output().unwrap();
        assert_eq!(out.status.code(), Some(127), "{mode}");
        assert!(text(&out.stderr).starts_with("cactup: /nonexistent/bin/gcc: "), "{mode}: {}", text(&out.stderr));
    }
    // No compiler at all is not something a recipe can ask for.
    let build = Build::new("record");
    let out = Command::new(CACTUP).arg("__cc").arg(build.conf()).env_remove("CACTUP_CC_CMD").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}

/// Meaningful in a debug build only: the release binary has no test panics.
#[cfg(debug_assertions)]
#[test]
fn a_panic_never_costs_the_compile() {
    let build = Build::new("record");
    // Before the compiler has run: this process becomes the compiler.
    let out = build.wrap("sh", &["-c", COMPILE]).env("CACTUP_CC_TEST_PANIC", "before").output().unwrap();
    assert_ran(&out, "out\n", "err\n", 3);
    // After: the compiler's exit status stands.
    let out = build.wrap("sh", &["-c", COMPILE]).env("CACTUP_CC_TEST_PANIC", "after").output().unwrap();
    assert_ran(&out, "out\n", "err\n", 3);
}

#[test]
fn the_selftest_compiler_answers_whether_a_compile_could_be_wrapped() {
    let build = Build::new("record");
    assert_ran(&build.wrap("cactup:selftest", &["-c", "a.c"]).output().unwrap(), "", "", 0);
    assert!(build.events().is_empty());
    fs::remove_file(build.conf()).unwrap();
    let out = build.wrap("cactup:selftest", &["-c", "a.c"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).starts_with("cactup: Failed to read "), "{}", text(&out.stderr));
}

#[test]
fn an_ordinary_command_is_untouched() {
    let out = Command::new(CACTUP).arg("--version").output().unwrap();
    assert!(out.status.success());
    assert!(text(&out.stdout).starts_with("cactup "), "{}", text(&out.stdout));
}

/// The makes to test against: the one on `PATH`, and any named in
/// `CACTUP_TEST_MAKES` (colon-separated paths) — the fragment has to hold
/// across GNU make versions, and a development host has only one.
fn makes() -> Vec<PathBuf> {
    let listed = std::env::var_os("CACTUP_TEST_MAKES").unwrap_or_default();
    let mut makes: Vec<PathBuf> = std::env::split_paths(&listed).filter(|p| !p.as_os_str().is_empty()).collect();
    makes.push(PathBuf::from("make"));
    makes.retain(|make| Command::new(make).arg("--version").output().is_ok_and(|o| o.status.success()));
    makes
}

/// A fake compiler that records its command line in `log` and, like a
/// compiler, writes the file named after `-o`.
fn fake_compiler(path: &Path, log: &Path, name: &str) {
    executable(
        path,
        &format!(
            "#!/bin/sh\necho \"{name} $*\" >> '{}'\nwhile [ $# -gt 0 ]; do [ \"$1\" = -o ] && : > \"$2\"; shift; done\n",
            log.display()
        ),
    );
}

/// A miniature of Cactus's object sub-make, with its includes in Cactus's
/// order: the configuration's definitions, the thorn's `make.code.defn`,
/// the rules, the thorn's `make.code.deps`. The objects hang off a
/// prerequisite whose recipe stands in for an ExternalLibraries `build.sh`:
/// it reads `CC` from its environment and starts a third-party make.
const MAKE_SUBDIR: &str = "\
include $(CONFIG)/make.config.defn
-include $(SRCDIR)/make.code.defn
OBJS = a.c.o b.cc.o
$(CCTK_TARGET): $(OBJS) a.c.d
$(OBJS): external-done
external-done:
\t@echo \"external CC=$$CC MAKEFILES=[$$MAKEFILES]\" >> $(LOG)
\t@$(MAKE) -s -C $(FOREIGN) LOG=$(LOG)
include $(CONFIG)/make.config.rules
-include $(SRCDIR)/make.code.deps
";

/// Compile recipes in the shape of Cactus's `make.config.rules`.
const MAKE_CONFIG_RULES: &str = "\
define COMPILE_C
current_wd=`$(GET_WD)` ; cd $(SCRATCH_BUILD) ; $(CC) $(CFLAGS) -c -o $$current_wd/$@ $$current_wd/$(notdir $<)
endef
define COMPILE_CXX
current_wd=`$(GET_WD)` ; cd $(SCRATCH_BUILD) ; $(CXX) -c -o $$current_wd/$@ $$current_wd/$(notdir $<)
endef
%.c.o: $(SRCDIR)/%.c
\t@cp $< .
\t@$(COMPILE_C)
%.cc.o: $(SRCDIR)/%.cc
\t@cp $< .
\t@$(COMPILE_CXX)
%.c.d: $(SRCDIR)/%.c
\t@$(CC) -E -M $(notdir $<)
";

/// A third-party makefile with its own compiler, an object named the way
/// Cactus names objects, and the "where am I" idiom.
const FOREIGN_MAKEFILE: &str = "\
CC = $(FOREIGN_CC)
all: lib.c.o
\t@echo \"foreign makefile=$(notdir $(firstword $(MAKEFILE_LIST))) MAKEFILES=[$$MAKEFILES]\" >> $(LOG)
lib.c.o:
\t@$(CC) -c lib.c -o $@
";

/// The fake Cactus tree of [`Build`], filled in far enough to build.
struct Tree<'a> {
    build: &'a Build,
    log: PathBuf,
    src: PathBuf,
}

impl<'a> Tree<'a> {
    fn new(build: &'a Build) -> Self {
        let root = &build.root;
        let log = root.join("log");
        for name in ["cc", "cxx", "thorn-cc", "foreign-cc"] {
            fake_compiler(&root.join(name), &log, name);
        }
        let config_data = build.config.join("config-data");
        fs::write(
            config_data.join("make.config.defn"),
            format!(
                "export CC = {}\nexport CXX = {} -std=c++17\nexport F90 = none\nGET_WD = pwd\n\
                 export SCRATCH_BUILD = {}\nexport FOREIGN_CC = {}\n",
                root.join("cc").display(),
                root.join("cxx").display(),
                build.config.join("scratch").display(),
                root.join("foreign-cc").display(),
            ),
        )
        .unwrap();
        fs::write(config_data.join("make.config.rules"), MAKE_CONFIG_RULES).unwrap();
        fs::create_dir_all(root.join("lib/make")).unwrap();
        fs::write(root.join("lib/make/make.subdir"), MAKE_SUBDIR).unwrap();
        fs::create_dir_all(root.join("foreign")).unwrap();
        fs::write(root.join("foreign/Makefile"), FOREIGN_MAKEFILE).unwrap();
        let src = root.join("arrangements/A/Thorn/src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("a.c"), "").unwrap();
        fs::write(src.join("b.cc"), "").unwrap();
        Self { build, log, src }
    }

    fn inject(&self) -> PathBuf {
        self.build.cc.join("inject.mk")
    }

    /// Run the probe; its exit status and stderr.
    fn probe(&self) -> Output {
        Command::new(CACTUP).arg("__cc-probe").arg(self.build.conf()).output().unwrap()
    }

    /// The build script's self-test (`Staged::probe_step`), by hand.
    fn selftest(&self, make: &Path) -> bool {
        let run = |dir: &Path, makefile: &str| {
            let out = Command::new(make)
                .current_dir(dir)
                .args(["-s", "-f"])
                .arg(self.build.cc.join("selftest").join(makefile))
                .args(["CCTK_TARGET=cactup-selftest", "SRCDIR=."])
                .env("MAKEFILES", self.inject())
                .output()
                .unwrap();
            out.status.success()
        };
        run(&self.build.config.join("build"), "wrapped.mk") && run(&self.build.cc, "untouched.mk")
    }

    /// Run the object sub-make in a fresh build directory, with the
    /// fragment (or without); the sorted log, paths shortened.
    fn make(&self, make: &Path, dir: &str, fragment: bool) -> Vec<String> {
        let cwd = self.build.config.join("build").join(dir);
        fs::create_dir_all(&cwd).unwrap();
        let mut cmd = Command::new(make);
        cmd.current_dir(&cwd).args(["-s", "-f"]).arg(self.build.root.join("lib/make/make.subdir"));
        cmd.arg("CCTK_TARGET=make.checked");
        cmd.arg(format!("CONFIG={}", self.build.config.join("config-data").display()));
        cmd.arg(format!("SRCDIR={}", self.src.display()));
        cmd.arg(format!("LOG={}", self.log.display()));
        cmd.arg(format!("FOREIGN={}", self.build.root.join("foreign").display()));
        cmd.env_remove("CC").env_remove("CXX").env_remove("MAKEFILES").env_remove("MAKEFLAGS");
        if fragment {
            cmd.env("MAKEFILES", self.inject());
        }
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{}{}", text(&out.stdout), text(&out.stderr));
        let log = fs::read_to_string(&self.log).unwrap();
        fs::remove_file(&self.log).unwrap();
        let _ = fs::remove_file(self.build.root.join("foreign/lib.c.o"));
        let mut lines: Vec<String> = log
            .lines()
            .map(|l| l.replace(&format!("{}/", cwd.display()), "").replace(&format!("{}/", self.build.root.display()), ""))
            .collect();
        lines.sort();
        lines
    }
}

#[test]
fn under_make_only_cactus_object_compiles_go_through_the_wrapper() {
    for make in makes() {
        let version = text(&Command::new(&make).arg("--version").output().unwrap().stdout);
        let version = version.lines().next().unwrap_or_default().to_owned();
        let build = Build::under("et_2026-05+x@y=z~", "record");
        let tree = Tree::new(&build);
        assert_ran(&tree.probe(), "", "", 0);
        assert!(tree.selftest(&make), "the self-test fails under {version}");

        // What the build does without the fragment is the reference.
        let plain = tree.make(&make, "plain", false);
        assert_eq!(
            plain,
            [
                "cc -E -M a.c",
                "cc -c -o a.c.o a.c",
                "cxx -std=c++17 -c -o b.cc.o b.cc",
                "external CC=cc MAKEFILES=[]",
                "foreign makefile=Makefile MAKEFILES=[]",
                "foreign-cc -c lib.c -o lib.c.o",
            ],
            "{version}"
        );
        assert!(build.events().is_empty());

        // With it, every compiler ran exactly as before, the prerequisite's
        // script and the third-party make saw nothing of it …
        assert_eq!(tree.make(&make, "wrapped", true), plain, "{version}");
        // … and the two object compiles, and only they, went through cactup.
        let events = build.events();
        assert_eq!(events.len(), 2, "{version}: {events:?}");
        assert!(events.iter().any(|e| e.contains("/cc\"")) && events.iter().any(|e| e.contains("/cxx\"")), "{events:?}");

        // A thorn's own compiler is the compiler, however the thorn sets it.
        for (dir, defn) in [("thorn-global", "CC := $(dir $(CC))thorn-cc\n"), ("thorn-pattern", "%.o: CC := $(dir $(CC))thorn-cc\n")] {
            fs::write(tree.src.join("make.code.defn"), defn).unwrap();
            let with = tree.make(&make, dir, true);
            let without = tree.make(&make, &format!("{dir}-plain"), false);
            assert_eq!(with, without, "{version}: {dir}");
            assert!(with.contains(&"thorn-cc -c -o a.c.o a.c".to_owned()), "{version}: {dir}: {with:?}");
        }
        fs::remove_file(tree.src.join("make.code.defn")).unwrap();
        assert_eq!(build.events().len(), 6, "{version}: both thorn builds were wrapped too");

        // A thorn with a compile recipe of its own keeps it: the fragment
        // stands down for that directory.
        fs::write(tree.src.join("make.code.deps"), "define COMPILE_C\n$(CC) --thorn-recipe -o $@\nendef\n").unwrap();
        let with = tree.make(&make, "thorn-recipe", true);
        assert_eq!(with, tree.make(&make, "thorn-recipe-plain", false), "{version}");
        assert!(with.contains(&"cc --thorn-recipe -o a.c.o".to_owned()), "{version}: {with:?}");
        assert_eq!(build.events().len(), 6, "{version}: nothing of that directory is wrapped");
    }
}

#[test]
fn the_probe_declines_with_one_line_and_its_own_status() {
    let declined = |out: &Output, why: &str| {
        assert_eq!(out.status.code(), Some(3));
        let stderr = text(&out.stderr);
        assert!(stderr.starts_with("cactup: build cache off for this build: "), "{stderr}");
        assert!(stderr.contains(why), "{stderr}");
        assert_eq!(stderr.lines().count(), 1, "{stderr}");
    };

    // Not configured yet: no rules to read.
    let build = Build::new("record");
    declined(&Command::new(CACTUP).arg("__cc-probe").arg(build.conf()).output().unwrap(), "make.config.rules");
    assert!(!build.cc.join("inject.mk").exists());

    // A path make would take apart.
    for parent in ["my (old) trees", "et,2026"] {
        let build = Build::under(parent, "record");
        let tree = Tree::new(&build);
        declined(&tree.probe(), "does not pass through make");
        assert!(!tree.inject().exists());
    }

    declined(&Command::new(CACTUP).arg("__cc-probe").output().unwrap(), "without a configuration file");
}
