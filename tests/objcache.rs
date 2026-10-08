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
        Self::named(parent, "sim", mode)
    }

    /// [`Self::under`], with the configuration called `config`.
    fn named(parent: &str, config: &str, mode: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        // make compares physical paths; so does the probe.
        let root = fs::canonicalize(tmp.path()).unwrap().join(parent).join("Cactus");
        let config = root.join("configs").join(config);
        let cc = config.join(".cactup-builds/0000/cc");
        for dir in [&cc, &config.join("config-data"), &config.join("build/Thorn"), &config.join("scratch")] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(
            cc.join("config.toml"),
            format!(
                "mode = \"{mode}\"\ncactup = \"{CACTUP}\"\nconfig-dir = \"{}\"\ncactus-root = \"{}\"\n\
                 machine = \"test\"\nbuild-env-digest = \"\"\nstore = \"{}\"\nrelocate = true\n",
                config.display(),
                root.display(),
                root.parent().unwrap().join("store").display()
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

    /// `cactup __cc <conf> <compiler> <shell> <args…>`, the way an injected
    /// recipe runs it.
    fn wrap(&self, compiler: &str, args: &[&str]) -> Command {
        self.wrap_under(Path::new("/bin/sh"), compiler, args)
    }

    /// [`Self::wrap`], with `shell` as the recipe's shell.
    fn wrap_under(&self, shell: &Path, compiler: &str, args: &[&str]) -> Command {
        let mut cmd = Command::new(CACTUP);
        cmd.arg("__cc").arg(self.conf()).arg(compiler).arg(shell).args(args);
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

/// Make the file at `path` executable, as a fresh file that a child
/// process (`install`) wrote. A test that writes an executable and runs it
/// at once fails now and then with "Text file busy": another test thread
/// that forks in between hands its child this process's open handle on the
/// file, and the kernel will not run a file open for writing. A file only
/// a child process ever had open for writing has no such handle here.
fn make_executable(path: &Path) {
    let fresh = path.with_extension("cactup-fresh");
    let installed = Command::new("install").arg("-m").arg("755").arg(path).arg(&fresh).status().unwrap();
    assert!(installed.success(), "install {}", path.display());
    fs::rename(&fresh, path).unwrap();
}

fn executable(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
    make_executable(path);
}

const COMPILE: &str = "echo out; echo err >&2; exit 3";

#[test]
fn without_a_readable_configuration_it_is_just_the_compiler() {
    let out = Command::new(CACTUP)
        .args(["__cc", "/nonexistent/cc/config.toml", "sh", "/bin/sh", "-c", COMPILE])
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

        let out = build.wrap("sh", &["-c", "echo \"[$KEPT]\""]).env("KEPT", "yes").output().unwrap();
        assert_ran(&out, "[yes]\n", "", 0);
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
    let out = build.wrap_under(&shell, "FOO=1 gcc", &["-c", "a.c"]).output().unwrap();
    assert_ran(&out, &format!("recipe shell got: -c FOO=1 gcc \"$@\" {} -c a.c\n", shell.display()), "", 0);
    // Each is on record as left to the shell, with nothing about how it went.
    let events = build.events();
    assert_eq!(events.len(), 3, "{events:?}");
    assert!(events.iter().all(|e| e.contains("\"not_cached\":\"the compiler is not a plain command") && !e.contains("exit")));
}

#[test]
fn a_compiler_already_behind_another_wrapper_is_left_to_it() {
    let build = Build::new("record");
    let ccache = build.root.join("ccache");
    executable(&ccache, "#!/bin/sh\necho \"ccache ran: $*\"\n");
    let out = build.wrap(&format!("{} gcc", ccache.display()), &["-c", "a.c"]).output().unwrap();
    assert_ran(&out, "ccache ran: gcc -c a.c\n", "", 0);
    assert!(build.events()[0].contains("already runs through another wrapper"));
}

/// A name the recipe's shell would not take from `PATH` — because a function
/// of that name was exported to it, or because `PATH` has a `~` entry that
/// only the shell expands — is the shell's to resolve, even when a program
/// of that name could be started directly.
#[test]
fn a_compiler_the_shell_resolves_differently_is_left_to_the_shell() {
    let bash = Path::new("/bin/bash");
    if !bash.exists() {
        eprintln!("skipped: no /bin/bash on this host");
        return;
    }
    let build = Build::new("record");
    let (bin, home_bin) = (build.root.join("bin"), build.root.join("home/bin"));
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&home_bin).unwrap();
    executable(&bin.join("mycc"), "#!/bin/sh\necho \"the program on PATH: $*\"\n");
    executable(&home_bin.join("mycc"), "#!/bin/sh\necho \"the program in ~/bin: $*\"\n");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());

    // An exported function shadows the program.
    let out = build
        .wrap_under(bash, "mycc -O2", &["-c", "a.c"])
        .env("PATH", &path)
        .env("BASH_FUNC_mycc%%", "() { echo \"the function: $*\"; }")
        .output()
        .unwrap();
    assert_ran(&out, "the function: -O2 -c a.c\n", "", 0);

    // A `~` entry in PATH: whatever this bash makes of it is the answer.
    let tilde_path = format!("~/bin:{path}");
    let home = build.root.join("home").display().to_string();
    let env = [("PATH", tilde_path.as_str()), ("HOME", home.as_str())];
    let reference = Command::new(bash).args(["-c", "mycc \"$@\"", "bash", "-c", "a.c"]).envs(env).output().unwrap();
    let out = build.wrap_under(bash, "mycc", &["-c", "a.c"]).envs(env).output().unwrap();
    assert_ran(&out, &text(&reference.stdout), "", 0);

    // Neither was this process's to start: both are on record as left to
    // the shell. The plain case is started here.
    let events = build.events();
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|e| e.contains("the recipe's shell runs it")), "{events:?}");
    let out = build.wrap_under(bash, "mycc", &["-c", "a.c"]).env("PATH", &path).output().unwrap();
    assert_ran(&out, "the program on PATH: -c a.c\n", "", 0);
    assert!(build.events()[2].contains("\"exit\":0"));
}

/// What bash sets up for itself at startup, from the file `BASH_ENV` names
/// (module systems set it), is invisible from outside: the wrapper asks the
/// shell, and a compiler name it gives another meaning runs as the shell
/// runs it. The answer is remembered for the attempt until what it depends
/// on changes.
#[test]
fn a_compiler_name_the_shell_redefines_at_startup_is_left_to_the_shell() {
    let bash = Path::new("/bin/bash");
    if !bash.exists() {
        eprintln!("skipped: no /bin/bash on this host");
        return;
    }
    let build = Build::new("record");
    let bin = build.root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    executable(&bin.join("mycc"), "#!/bin/sh\necho \"the program on PATH: $*\"\n");
    executable(&bin.join("othercc"), "#!/bin/sh\necho \"another program: $*\"\n");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let startup = build.root.join("bash_env");
    let asked = build.root.join("asked");
    // Counts how often a bash started (each one reads the file).
    let counting = format!("echo x >> '{}'\n", asked.display());
    let run = || build.wrap_under(bash, "mycc", &["-c", "a.c"]).env("PATH", &path).env("BASH_ENV", &startup).output().unwrap();

    for (what, defines, runs) in [
        ("a function", "mycc() { echo \"the function: $*\"; }\n", "the function: -c a.c\n"),
        ("an alias", "shopt -s expand_aliases\nalias mycc='echo the alias:'\n", "the alias: -c a.c\n"),
        ("a remembered path", &format!("hash -p '{}' mycc\n", bin.join("othercc").display()), "another program: -c a.c\n"),
    ] {
        fs::write(&startup, defines).unwrap();
        assert_ran(&run(), runs, "", 0);
        let events = build.events();
        let last = events.last().unwrap();
        assert!(last.contains("mycc is something else to the shell"), "{what}: {last}");
    }

    // A startup file that leaves the name alone: the program on PATH,
    // started here and keyed as usual, and the shell asked only once.
    let _ = fs::remove_file(&asked);
    fs::write(&startup, &counting).unwrap();
    let before = build.events().len();
    for _ in 0..3 {
        assert_ran(&run(), "the program on PATH: -c a.c\n", "", 0);
    }
    let events = build.events();
    assert_eq!(events.len(), before + 3);
    assert!(events[before..].iter().all(|e| e.contains("\"exit\":0")), "{events:?}");
    assert_eq!(fs::read_to_string(&asked).unwrap().lines().count(), 1, "the answer was not remembered");

    // bash runs a function named by a path, too, where the recipe names the
    // compiler by that path.
    let by_path = bin.join("mycc").display().to_string();
    fs::write(&startup, format!("function {by_path} {{ echo \"the function by its path: $*\"; }}\n")).unwrap();
    let out = build.wrap_under(bash, &by_path, &["-c", "a.c"]).env("PATH", &path).env("BASH_ENV", &startup).output().unwrap();
    assert_ran(&out, "the function by its path: -c a.c\n", "", 0);
    assert!(build.events().last().unwrap().contains("is something else to the shell"));

    // Until the startup file changes: then the shell is asked again.
    fs::write(&startup, format!("{counting}mycc() {{ echo \"the function: $*\"; }}\n")).unwrap();
    let out = run();
    assert!(text(&out.stdout).ends_with("the function: -c a.c\n"), "{}", text(&out.stdout));
    assert!(build.events().last().unwrap().contains("mycc is something else to the shell"));
}

/// What the recipe's shell can start and this process cannot, the shell
/// starts: the build must not fail where it works without cactup.
#[test]
fn a_compiler_only_a_shell_can_start_is_handed_to_the_shell() {
    for mode in ["off", "record"] {
        let build = Build::new(mode);
        let bin = build.root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        // An executable script without an interpreter line: the kernel will
        // not run it, a shell will. Named by path, and found through PATH.
        executable(&bin.join("sitecc"), "echo \"site compiler: $*\"\n");
        let by_path = bin.join("sitecc").display().to_string();
        let out = build.wrap(&by_path, &["-c", "a.c"]).output().unwrap();
        assert_ran(&out, "site compiler: -c a.c\n", "", 0);
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
        let out = build.wrap("sitecc -O2", &["-c", "a b.c"]).env("PATH", &path).output().unwrap();
        assert_ran(&out, "site compiler: -O2 -c a b.c\n", "", 0);

        // A shell builtin in front of the compiler (`command`, `exec`, and
        // under bash the keyword `time`).
        let out = build.wrap("command printf", &["%s|", "-c", "a b.c"]).output().unwrap();
        assert_ran(&out, "-c|a b.c|", "", 0);
        let out = build.wrap("exec printf", &["%s|", "-c", "a b.c"]).output().unwrap();
        assert_ran(&out, "-c|a b.c|", "", 0);

        // A shell function, exported the way bash exports them.
        if Path::new("/bin/bash").exists() {
            let out = build
                .wrap_under(Path::new("/bin/bash"), "mycc -O2", &["-c", "a.c"])
                .env("BASH_FUNC_mycc%%", "() { echo \"function compiler: $*\"; }")
                .output()
                .unwrap();
            assert_ran(&out, "function compiler: -O2 -c a.c\n", "", 0);
        }
    }
}

#[test]
fn it_dies_of_the_signal_that_stopped_the_compiler() {
    // A signal this test was started ignoring (a background job of a
    // non-interactive shell ignores SIGINT and SIGQUIT) reaches the compiler
    // ignored, rightly: such a case says nothing here.
    let status = fs::read_to_string("/proc/self/status").unwrap();
    let ignored = status.lines().find_map(|line| line.strip_prefix("SigIgn:")).unwrap();
    let ignored = u64::from_str_radix(ignored.trim(), 16).unwrap();
    let ignoring = |signal: u32| ignored & (1 << (signal - 1)) != 0;
    let build = Build::new("record");
    if !ignoring(15) {
        let status = build.wrap("sh", &["-c", "kill -TERM $$"]).status().unwrap();
        assert_eq!(status.signal(), Some(15), "{status:?}");
        assert!(build.events()[0].contains("\"signal\":15"));
    }
    // SIGQUIT and a crashed compiler are exit codes, not core files of cactup.
    if !ignoring(3) {
        let status = build.wrap("sh", &["-c", "kill -QUIT $$"]).status().unwrap();
        assert_eq!(status.code(), Some(128 + 3), "{status:?}");
    }
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
        cmd.args(["-c", "trap '' HUP; exec \"$@\"", "sh", CACTUP, "__cc"]).arg(build.conf());
        cmd.args(["sh", "/bin/sh", "-c", compile]).output().unwrap()
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
fn a_compiler_that_cannot_start_fails_as_it_does_in_the_shell() {
    let direct = Command::new("/bin/sh").args(["-c", "/nonexistent/bin/gcc \"$@\"", "/bin/sh", "-c", "a.c"]).output().unwrap();
    assert_eq!(direct.status.code(), Some(127));
    for mode in ["off", "record"] {
        // The message and the status are the shell's own, as without cactup.
        let build = Build::new(mode);
        let out = build.wrap("/nonexistent/bin/gcc", &["-c", "a.c"]).output().unwrap();
        assert_ran(&out, "", &text(&direct.stderr), 127);
        // A recording build says what became of the compile: the shell's.
        let events = build.events();
        match mode {
            "record" => assert!(events.len() == 1 && events[0].contains("the recipe's shell runs it"), "{events:?}"),
            _ => assert!(events.is_empty(), "{events:?}"),
        }
    }
    // No compiler at all is not something a recipe can ask for.
    let build = Build::new("record");
    let out = Command::new(CACTUP).arg("__cc").arg(build.conf()).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out.stderr).starts_with("cactup: "), "{}", text(&out.stderr));
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
fn an_empty_compiler_runs_what_the_recipe_would_have_run() {
    // `$(CC)` expanding to nothing leaves the recipe running its first
    // argument as the command. So does the wrapper.
    let build = Build::new("record");
    assert_ran(&build.wrap("", &["echo", "ran"]).output().unwrap(), "ran\n", "", 0);
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

    /// The build script's self-test (`Staged::probe_step`), by hand: each
    /// run's `all`, and the file it leaves when its checks all ran.
    fn selftest(&self, make: &Path) -> bool {
        let selftest = self.build.cc.join("selftest");
        let build = self.build.config.join("build");
        let run = |dir: &Path, makefile: &str, args: &[&str], passed: &str| {
            let out = Command::new(make)
                .current_dir(dir)
                .args(["-s", "-f"])
                .arg(selftest.join(makefile))
                .args(["all", "SRCDIR=."])
                .args(args)
                .env("MAKEFILES", self.inject())
                .output()
                .unwrap();
            out.status.success() && selftest.join(passed).is_file()
        };
        run(&build, "wrapped.mk", &["CCTK_TARGET=cactup-selftest"], "wrapped.passed")
            && run(&build, "untouched.mk", &["RUN=elsewhere"], "elsewhere.passed")
            && run(&self.build.cc, "untouched.mk", &["RUN=untouched", "CCTK_TARGET=cactup-selftest"], "untouched.passed")
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
        cmd.env_remove("CC").env_remove("CXX").env_remove("MAKEFLAGS");
        // The user has a MAKEFILES of their own, with or without the cache.
        let user = self.build.root.join("user.mk");
        fs::write(&user, "").unwrap();
        let mut makefiles = user.into_os_string();
        if fragment {
            makefiles.push(" ");
            makefiles.push(self.inject());
        }
        cmd.env("MAKEFILES", makefiles);
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
        let build = Build::under("et_2026-05+x@y~", "record");
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
                "external CC=cc MAKEFILES=[user.mk]",
                "foreign makefile=user.mk MAKEFILES=[user.mk]",
                "foreign-cc -c lib.c -o lib.c.o",
            ],
            "{version}"
        );
        assert!(build.events().is_empty());

        // With it, every compiler ran exactly as before, and the
        // prerequisite's script and the third-party make saw nothing of it:
        // not in their environment, not in MAKEFILE_LIST …
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
        fs::remove_file(tree.src.join("make.code.deps")).unwrap();

        // So does one that could define a recipe where the fragment cannot
        // see it: the build is the plain one, and nothing is wrapped. One
        // that only looks like it is wrapped as before.
        fs::write(tree.src.join("extra.mk"), "").unwrap();
        for (i, (file, text)) in [
            ("make.code.defn", "include $(SRCDIR)/extra.mk\n"),
            ("make.code.deps", "-include $(SRCDIR)/missing.mk\n"),
            ("make.code.defn", "define UNUSED\nx\nendef\n"),
            ("make.code.deps", "$(eval UNUSED := 1)\n"),
            // make joins a line ending in a backslash to the next first.
            ("make.code.deps", "include\\\n$(SRCDIR)/extra.mk\n"),
            ("make.code.defn", "UNUSED := $(eval\\\n$(file <$(SRCDIR)/extra.mk))\n"),
        ]
        .into_iter()
        .enumerate()
        {
            fs::write(tree.src.join(file), text).unwrap();
            let with = tree.make(&make, &format!("indirect-{i}"), true);
            assert_eq!(with, plain, "{version}: {text:?}");
            assert_eq!(build.events().len(), 6, "{version}: {text:?} was wrapped");
            fs::remove_file(tree.src.join(file)).unwrap();
        }
        // A file that cannot be read might say anything.
        let unreadable = tree.src.join("make.code.deps");
        fs::write(&unreadable, "").unwrap();
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0)).unwrap();
        if fs::read(&unreadable).is_err() {
            assert_eq!(tree.make(&make, "unreadable", true), plain, "{version}");
            assert_eq!(build.events().len(), 6, "{version}: a thorn with an unreadable make.code.deps was wrapped");
        }
        fs::remove_file(&unreadable).unwrap();
        fs::write(tree.src.join("make.code.defn"), "INCLUDE_DIRS += include\n# include and define, in words\n").unwrap();
        assert_eq!(tree.make(&make, "words", true), plain, "{version}");
        assert_eq!(build.events().len(), 8, "{version}: words alone stood the fragment down");
        fs::remove_file(tree.src.join("make.code.defn")).unwrap();
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
    for parent in ["my (old) trees", "et,2026", "gcc=13"] {
        let build = Build::under(parent, "record");
        let tree = Tree::new(&build);
        declined(&tree.probe(), "does not pass through make");
        assert!(!tree.inject().exists());
    }

    declined(&Command::new(CACTUP).arg("__cc-probe").output().unwrap(), "without a configuration file");
}

// ---------------------------------------------------------------------------
// Keys, with real compilers.

/// Is `compiler` on this host, and one the cache keys? (A test that needs it
/// says so and passes without it.)
fn have(compiler: &str) -> bool {
    let build = Build::new("record");
    fs::write(build.config.join("build/Thorn/probe.c"), "int probe;\n").unwrap();
    let ran = build.wrap(compiler, &["-c", "-o", "probe.o", "probe.c"]).current_dir(build.config.join("build/Thorn")).output();
    let keyed = ran.is_ok_and(|out| out.status.success()) && build.events().first().is_some_and(|e| e.contains("\"key\""));
    if !keyed {
        eprintln!("skipped: no {compiler} on this host that the cache keys");
    }
    keyed
}

/// One Cactus-shaped compile: a thorn's source copied into the build
/// directory with a line directive naming where it came from, a header from
/// the thorn's source directory, and one from outside the tree.
struct Unit<'a> {
    build: &'a Build,
    source: PathBuf,
    object: PathBuf,
    header: PathBuf,
}

const UNIT_HEADER: &str = "static inline int twice(int x) { return 2 * x; } /* doubled */\n";
const UNIT_SOURCE: &str = "#include \"unit.h\"\n#include \"lib.h\"\n#include <assert.h>\n\
    const char *where(void) { return __FILE__; }\n\
    int sum(int n, const int *v) {\n  int s = LIB_START;\n\
    #pragma omp parallel for reduction(+:s)\n  for (int i = 0; i < n; i++) s += twice(v[i]);\n\
    \x20 assert(s >= 0);\n  return s;\n}\n";

impl<'a> Unit<'a> {
    /// `lib` is a directory outside every tree, the same for all of them.
    fn new(build: &'a Build, suffix: &str, lib: &Path) -> Self {
        Self::with_line_directive(build, suffix, lib, true)
    }

    /// With or without the line directive Cactus begins a build copy with
    /// when the option list asks for line directives.
    fn with_line_directive(build: &'a Build, suffix: &str, lib: &Path, line_directive: bool) -> Self {
        let thorn = build.root.join("arrangements/Arr/Thorn/src");
        fs::create_dir_all(&thorn).unwrap();
        let original = thorn.join(format!("unit.{suffix}"));
        let source = build.config.join(format!("build/Thorn/unit.{suffix}"));
        fs::write(&original, UNIT_SOURCE).unwrap();
        let directive = if line_directive { format!("#line 1 \"{}\"\n", original.display()) } else { String::new() };
        fs::write(&source, format!("{directive}{UNIT_SOURCE}")).unwrap();
        fs::write(thorn.join("unit.h"), UNIT_HEADER).unwrap();
        fs::write(thorn.join("forced.h"), "#define FORCED 1 /* for -include */\n").unwrap();
        fs::write(lib.join("lib.h"), "#define LIB_START 0\n").unwrap();
        let object = build.config.join(format!("build/Thorn/unit.{suffix}.o"));
        Self { build, source, object, header: thorn.join("unit.h") }
    }

    fn args(&self, flags: &[&str], lib: &Path) -> Vec<String> {
        let mut args: Vec<String> = flags.iter().map(|flag| (*flag).to_owned()).collect();
        let path = |path: &Path| path.display().to_string();
        args.extend(["-c".to_owned(), "-o".to_owned(), path(&self.object), path(&self.source)]);
        args.push(format!("-I{}", path(self.header.parent().unwrap())));
        args.push(format!("-I{}", path(lib)));
        args
    }

    /// Compile through the wrapper from `dir` (below the configuration),
    /// and return what it logged: the key, and whether another tree can
    /// share it. `None`: not keyed.
    fn keyed_from(&self, dir: &str, compiler: &str, flags: &[&str], lib: &Path) -> Option<(String, bool)> {
        let before = self.build.events().len();
        let args = self.args(flags, lib);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let cwd = self.build.config.join(dir);
        let out = self.build.wrap(compiler, &args).current_dir(&cwd).env("PWD", &cwd).output().unwrap();
        assert!(out.status.success(), "{compiler} {flags:?}: {}", text(&out.stderr));
        let events = self.build.events();
        assert_eq!(events.len(), before + 1);
        let event: serde_json::Value = serde_json::from_str(&events[before]).unwrap();
        Some((event.get("key")?.as_str()?.to_owned(), event["relocatable"].as_bool()?))
    }

    fn keyed(&self, compiler: &str, flags: &[&str], lib: &Path) -> Option<(String, bool)> {
        self.keyed_from("scratch", compiler, flags, lib)
    }

    fn why_not(&self, compiler: &str, flags: &[&str], lib: &Path) -> String {
        assert_eq!(self.keyed(compiler, flags, lib), None);
        let event: serde_json::Value = serde_json::from_str(self.build.events().last().unwrap()).unwrap();
        event["not_cached"].as_str().unwrap().to_owned()
    }

    /// The object of the same compile run the way a serving cache will run
    /// it: with the Cactus root and the configuration directory mapped to
    /// fixed names, the configuration's map last. (Spelled out here as spec
    /// §18.5 gives them; `key::tests::maps_names_as_the_compiler_does` pins
    /// cactup's own spelling to the same strings.)
    fn mapped_object(&self, compiler: &str, flags: &[&str], lib: &Path) -> Vec<u8> {
        let mut args = self.args(flags, lib);
        args.push(format!("-ffile-prefix-map={}/=/cactup-root/", self.build.root.display()));
        args.push(format!("-ffile-prefix-map={}/=/cactup-root/configs/@config/", self.build.config.display()));
        let cwd = self.build.config.join("scratch");
        let out = Command::new(compiler).args(&args).current_dir(&cwd).env("PWD", &cwd).output().unwrap();
        assert!(out.status.success(), "{compiler} {args:?}: {}", text(&out.stderr));
        fs::read(&self.object).unwrap()
    }
}

/// This host's GCC driver, copied to `<prefix>/bin/gcc` with its back ends
/// linked in where it looks for them, and `specs` as the specs file it
/// finds there: a site-built GCC, as on qbd.
fn site_gcc(prefix: &Path, specs: &str) -> PathBuf {
    let ask = |arg: &str| text(&Command::new("gcc").arg(arg).output().unwrap().stdout).trim().to_owned();
    let cc1 = PathBuf::from(ask("-print-prog-name=cc1"));
    let (version, triple) = (cc1.parent().unwrap(), ask("-dumpmachine"));
    let libexec = prefix.join("libexec/gcc").join(&triple);
    let lib = prefix.join("lib/gcc").join(&triple).join(version.file_name().unwrap());
    for dir in [&prefix.join("bin"), &libexec, &lib] {
        fs::create_dir_all(dir).unwrap();
    }
    std::os::unix::fs::symlink(version, libexec.join(version.file_name().unwrap())).unwrap();
    let driver = prefix.join("bin/gcc");
    let gcc = text(&Command::new("sh").args(["-c", "command -v gcc"]).output().unwrap().stdout).trim().to_owned();
    fs::copy(fs::canonicalize(gcc).unwrap(), &driver).unwrap();
    make_executable(&driver);
    fs::write(lib.join("specs"), specs).unwrap();
    driver
}

/// A GCC whose specs file only adds an rpath to the link is keyed, and
/// compiles as the same GCC without the file does. One whose specs file
/// adds to a compile is not keyed.
#[test]
fn a_gcc_with_a_specs_file_that_only_changes_the_link_is_keyed() {
    if !have("gcc") {
        eprintln!("skipped: no GCC on this host");
        return;
    }
    let lib = tempfile::tempdir().unwrap();
    let lib = fs::canonicalize(lib.path()).unwrap();
    fs::write(lib.join("lib.h"), "#define LIB_START 0\n").unwrap();
    let build = Build::new("record");
    let unit = Unit::new(&build, "c", &lib);
    let builtin = text(&Command::new("gcc").arg("-dumpspecs").output().unwrap().stdout);
    let qbd = builtin.replacen("*link_libgcc:\n%D\n", "*link_libgcc:\n%(link_libgcc_rpath) %D\n", 1)
        + "*link_libgcc_rpath:\n-rpath /opt/gcc/lib64\n\n";
    assert!(qbd.contains("%(link_libgcc_rpath) %D"));

    let site = site_gcc(&build.root.join("site"), &qbd);
    let site = site.to_str().unwrap();
    assert!(unit.keyed(site, &["-O2", "-g"], &lib).is_some(), "{:?}", build.events().last());
    let with_specs = fs::read(&unit.object).unwrap();
    let args = unit.args(&["-O2", "-g"], &lib);
    let cwd = build.config.join("scratch");
    let out = Command::new("gcc").args(&args).current_dir(&cwd).env("PWD", &cwd).output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(fs::read(&unit.object).unwrap(), with_specs, "the specs file changed the object");

    let sneaky = site_gcc(&build.root.join("sneaky"), &builtin.replacen("*cc1:\n", "*cc1:\n-DSNEAKY ", 1));
    let why = unit.why_not(sneaky.to_str().unwrap(), &["-O2", "-g"], &lib);
    assert!(why.contains("reads a specs file") && why.contains("changes cc1"), "{why}");
}

/// The claim a cache stands on, tried for real: where two compiles in two
/// trees — elsewhere on disk, under another configuration name — share a
/// key, compiling them as a serving cache would gives one object, byte for
/// byte. And the cases meant to share a key do.
#[test]
fn compiles_that_share_a_key_produce_the_same_object() {
    let lib = tempfile::tempdir().unwrap();
    let lib = fs::canonicalize(lib.path()).unwrap();
    let mut shared = 0;
    for (compiler, suffix) in [("gcc", "c"), ("g++", "cc"), ("clang", "c"), ("clang++", "cc")] {
        if !have(compiler) {
            continue;
        }
        let clang = compiler.starts_with("clang");
        for flags in [
            &["-O2"][..],
            &["-g", "-O2"],
            &["-g", "-O0"],
            &["-g3", "-O0"],
            &["-gdwarf-4", "-O1"],
            &["-O2", "-fopenmp"],
            &["-g", "-O2", "-fopenmp"],
            &["-g", "-O2", "-fPIC", "-march=native", "-Wall"],
            &["-g", "-O2", "-DNDEBUG"],
        ] {
            // Build copies with a line directive and without: with one,
            // every name in the object is the original's; without, Clang's
            // debug information has a checksum of each file.
            for line_directive in [true, false] {
                let (here, there) = (Build::named("a", "sim", "record"), Build::named("b/deeper", "sim-debug", "record"));
                let unit = |build| Unit::with_line_directive(build, suffix, &lib, line_directive);
                let (ours, theirs) = (unit(&here), unit(&there));
                let what = format!("{compiler} {flags:?}, line directive: {line_directive}");
                // `-march=native` is not keyed on a host whose processors
                // differ: there the compiler itself makes two objects of
                // one compile, by the core it happens to run on.
                if flags.contains(&"-march=native") && ours.keyed(compiler, flags, &lib).is_none() {
                    let why = ours.why_not(compiler, flags, &lib);
                    assert!(why.contains("this host's processors are not all alike"), "{what}: {why}");
                    continue;
                }
                let (key, relocatable) = ours.keyed(compiler, flags, &lib).unwrap_or_else(|| panic!("{what}: not keyed"));
                let (their_key, _) = theirs.keyed(compiler, flags, &lib).unwrap_or_else(|| panic!("{what}: not keyed"));
                // Clang's OpenMP puts source paths where no map reaches:
                // such a key must know where the tree is. So must every
                // key of a compiler that failed the trial of the map (an
                // old one: the test has nothing to say against it).
                if clang && flags.contains(&"-fopenmp") {
                    assert!(!relocatable, "{what}");
                }
                assert_eq!(key == their_key, relocatable, "{what}");
                if key == their_key {
                    let (object, their_object) = (ours.mapped_object(compiler, flags, &lib), theirs.mapped_object(compiler, flags, &lib));
                    assert!(object == their_object, "{what}: one key, two objects");
                    // The same tree compiled again is the same object, too.
                    assert!(object == ours.mapped_object(compiler, flags, &lib), "{what}: not reproducible");
                    shared += 1;
                }
            }
        }
    }
    // Not for nothing: of the compilers this host has, some relocate.
    assert!(shared > 0 || !["gcc", "g++", "clang", "clang++"].iter().any(|compiler| have(compiler)), "no compile shared a key");
}

/// A recipe that has the compiler write its dependency file while it
/// compiles (`-MD -MP -MF <file> -MT <target>`): the same key and the same
/// object as without, and the file written by the compile alone — not by
/// the preprocessor runs that make the key, before or after.
#[test]
fn a_dependency_file_written_by_the_compile_changes_nothing() {
    let lib = tempfile::tempdir().unwrap();
    let lib = fs::canonicalize(lib.path()).unwrap();
    for (compiler, suffix) in [("gcc", "c"), ("g++", "cc"), ("clang", "c"), ("clang++", "cc")] {
        if !have(compiler) {
            continue;
        }
        let build = Build::new("record");
        let unit = Unit::new(&build, suffix, &lib);
        let (key, _) = unit.keyed(compiler, &["-g", "-O2"], &lib).unwrap();
        let object = fs::read(&unit.object).unwrap();

        let depfile = unit.object.with_extension("d");
        let (depfile_name, target) = (depfile.display().to_string(), unit.object.display().to_string());
        let flags = ["-g", "-O2", "-MD", "-MP", "-MF", &depfile_name, "-MT", &target];
        let (with_key, _) = unit.keyed(compiler, &flags, &lib).unwrap_or_else(|| panic!("{compiler}: not keyed"));
        assert_eq!(with_key, key, "{compiler}");
        assert!(fs::read(&unit.object).unwrap() == object, "{compiler}: the object changed");
        let written = fs::read_to_string(&depfile).unwrap();
        assert!(written.starts_with(&format!("{target}:")) && written.contains("unit.h"), "{compiler}: {written}");
        // Written once, by the compile: a file that is in the way of the
        // preprocessor runs is not written to.
        let event: serde_json::Value = serde_json::from_str(build.events().last().unwrap()).unwrap();
        assert_eq!(event["stable"], true, "{compiler}");
        // Nor after it: the check after the compile ran (the key was found
        // stable), and with GCC, which writes the object last, the file is
        // no newer than the object. (Clang writes the file last, so its
        // times say nothing. What both runs are given is pinned for both
        // families by a unit test of the preprocessor's command line.)
        if !compiler.starts_with("clang") {
            let modified = |path: &Path| fs::metadata(path).unwrap().modified().unwrap();
            assert!(modified(&depfile) <= modified(&unit.object), "{compiler}: the dependency file was written after the compile");
        }
        fs::remove_file(&depfile).unwrap();
        fs::create_dir(&depfile).unwrap();
        let scratch = build.config.join("scratch");
        let args = unit.args(&flags, &lib);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = build.wrap(compiler, &args).current_dir(&scratch).env("PWD", &scratch).output().unwrap();
        // The compile cannot write its file and says so; the key was made
        // all the same, without touching it.
        assert!(!out.status.success(), "{compiler}");
        let last = build.events().pop().unwrap();
        assert!(last.contains(&format!("\"key\":\"{key}\"")) && !last.contains("\"exit\":0"), "{compiler}: {last}");
    }
}

/// A path of the tree as the value of a flag. Where the compiler uses it
/// only to find files, the key has it mapped, and the objects must agree;
/// where the object keeps the flag as written (GCC records its command
/// line in debug information), two trees must not share a key.
#[test]
fn a_path_of_the_tree_in_a_flag_is_shared_only_where_the_object_does_not_keep_it() {
    let lib = tempfile::tempdir().unwrap();
    let lib = fs::canonicalize(lib.path()).unwrap();
    for (compiler, suffix) in [("gcc", "c"), ("g++", "cc"), ("clang", "c"), ("clang++", "cc")] {
        if !have(compiler) {
            continue;
        }
        let clang = compiler.starts_with("clang");
        let thorn = |unit: &Unit| unit.header.parent().unwrap().display().to_string();
        let rows: [(&dyn Fn(&Unit) -> Vec<String>, bool); 6] = [
            (&|unit| vec!["-isystem".into(), thorn(unit)], true),
            (&|unit| vec!["-iquote".into(), thorn(unit)], true),
            (&|unit| vec!["-idirafter".into(), thorn(unit)], true),
            (&|unit| vec!["-include".into(), format!("{}/forced.h", thorn(unit))], true),
            (&|unit| vec![format!("-frandom-seed={}", unit.source.display())], false),
            (&|unit| vec![format!("-fdebug-prefix-map={}=/elsewhere", unit.build.root.display())], false),
        ];
        for (tree_flags, shared) in rows {
            let (here, there) = (Build::named("a", "sim", "record"), Build::named("b/deeper", "sim-debug", "record"));
            let (ours, theirs) = (Unit::new(&here, suffix, &lib), Unit::new(&there, suffix, &lib));
            let flags = |unit: &Unit| [vec!["-g".to_owned(), "-O2".to_owned()], tree_flags(unit)].concat();
            let (our_flags, their_flags) = (flags(&ours), flags(&theirs));
            let (our_flags, their_flags): (Vec<&str>, Vec<&str>) =
                (our_flags.iter().map(String::as_str).collect(), their_flags.iter().map(String::as_str).collect());
            let what = format!("{compiler} {our_flags:?}");
            if clang && our_flags.contains(&"-include") {
                // Clang may take a precompiled header in the file's place.
                assert!(ours.why_not(compiler, &our_flags, &lib).contains("-include with Clang"), "{what}");
                continue;
            }
            let (key, relocatable) = ours.keyed(compiler, &our_flags, &lib).unwrap_or_else(|| panic!("{what}: not keyed"));
            let (their_key, _) = theirs.keyed(compiler, &their_flags, &lib).unwrap_or_else(|| panic!("{what}: not keyed"));
            let shared = shared && relocatable;
            assert_eq!(key == their_key, shared, "{what}");
            if shared {
                let (object, their_object) =
                    (ours.mapped_object(compiler, &our_flags, &lib), theirs.mapped_object(compiler, &their_flags, &lib));
                assert!(object == their_object, "{what}: one key, two objects");
            }
        }
    }
}

/// A header beside the tree, in a directory whose name only *begins* like
/// the tree's (`Cactus-libs` beside `Cactus`): the compiler does not map
/// it, and neither may the key.
#[test]
fn a_directory_that_begins_like_the_tree_is_not_the_tree() {
    for compiler in ["gcc", "clang"] {
        if !have(compiler) {
            continue;
        }
        let (here, there) = (Build::named("a", "sim", "record"), Build::named("b", "sim", "record"));
        // One library directory for both trees, named like `here`'s root.
        let lib = PathBuf::from(format!("{}-libs", here.root.display()));
        fs::create_dir_all(&lib).unwrap();
        let (ours, theirs) = (Unit::new(&here, "c", &lib), Unit::new(&there, "c", &lib));
        let flags = &["-g", "-O2"][..];
        let (key, _) = ours.keyed(compiler, flags, &lib).unwrap();
        let (their_key, _) = theirs.keyed(compiler, flags, &lib).unwrap();
        assert_eq!(key, their_key, "{compiler}: the library's path is the same absolute path for both");
        assert!(ours.mapped_object(compiler, flags, &lib) == theirs.mapped_object(compiler, flags, &lib), "{compiler}");
    }
}

/// What changes the object changes the key: the cases two reviews found
/// that an earlier key missed, each through the real wrapper.
#[test]
fn what_the_object_depends_on_is_in_the_key() {
    // The library's directory has a name compilers escape when they print
    // it (Clang writes the tab and the bytes outside ASCII in octal): a
    // name misread is a file not read.
    let lib = tempfile::tempdir().unwrap();
    let lib = fs::canonicalize(lib.path()).unwrap().join("bibliothèque\tà part");
    fs::create_dir_all(&lib).unwrap();
    for compiler in ["gcc", "clang"] {
        if !have(compiler) {
            continue;
        }
        let build = Build::new("record");
        let unit = Unit::new(&build, "c", &lib);
        let key = |flags: &[&str]| unit.keyed(compiler, flags, &lib).unwrap_or_else(|| panic!("{compiler} {flags:?}: not keyed")).0;
        let base = key(&["-g", "-O2"]);
        assert_eq!(key(&["-g", "-O2"]), base, "{compiler}: the same compile again");

        // Spacing, a comment, text the preprocessor drops: all of it is in
        // debug information one way or another (columns, checksums).
        let edits: [(&str, &dyn Fn(&str) -> String); 3] = [
            ("spacing", &|text| text.replace("int s = LIB_START;", "int  s  =  LIB_START;")),
            ("a comment", &|text| text.replace("return s;", "return s; /* the sum */")),
            ("skipped text", &|text| format!("{text}#if 0\nnever compiled\n#endif\n")),
        ];
        for (what, edit) in edits {
            let original = fs::read_to_string(&unit.source).unwrap();
            fs::write(&unit.source, edit(&original)).unwrap();
            assert_ne!(key(&["-g", "-O2"]), base, "{compiler}: {what} in the source");
            fs::write(&unit.source, &original).unwrap();
        }
        fs::write(&unit.header, UNIT_HEADER.replace("doubled", "twice over")).unwrap();
        assert_ne!(key(&["-g", "-O2"]), base, "{compiler}: a comment in a header");
        fs::write(&unit.header, UNIT_HEADER).unwrap();
        // Put back, the key is what it was: old entries stay good.
        assert_eq!(key(&["-g", "-O2"]), base, "{compiler}: reverted");

        // A header outside the tree: its tokens, and its bytes alone.
        fs::write(lib.join("lib.h"), "#define LIB_START 1\n").unwrap();
        assert_ne!(key(&["-g", "-O2"]), base, "{compiler}: a library header");
        fs::write(lib.join("lib.h"), "#define LIB_START 0 /* zero */\n").unwrap();
        assert_ne!(key(&["-g", "-O2"]), base, "{compiler}: a comment in a library header");
        fs::write(lib.join("lib.h"), "#define LIB_START 0\n").unwrap();
        assert_eq!(key(&["-g", "-O2"]), base, "{compiler}: reverted");

        // A header whose name begins like the compilers' names for what is
        // not a file, reached by a relative name: a file all the same.
        let odd = build.config.join("build/Thorn/<odd>.h");
        let original = fs::read_to_string(&unit.source).unwrap();
        fs::write(&unit.source, format!("{original}#include \"<odd>.h\"\n")).unwrap();
        fs::write(&odd, "static inline int odd(void) { return 1; } /* one */\n").unwrap();
        let from_build = |unit: &Unit| unit.keyed_from("build/Thorn", compiler, &["-g", "-O2"], &lib).unwrap().0;
        let with_odd = from_build(&unit);
        fs::write(&odd, "static inline int odd(void) { return 1; } /* 1 */\n").unwrap();
        assert_ne!(from_build(&unit), with_odd, "{compiler}: a comment in a header named <odd>.h");
        fs::write(&unit.source, &original).unwrap();

        // The directory the compile runs in, with debug information.
        let elsewhere = unit.keyed_from("build/Thorn", compiler, &["-g", "-O2"], &lib).unwrap().0;
        assert_ne!(elsewhere, base, "{compiler}: the working directory");
        let plain = key(&["-O2"]);
        assert_eq!(unit.keyed_from("build/Thorn", compiler, &["-O2"], &lib).unwrap().0, plain, "{compiler}: without it, not");

        // Flags the reader does not know for certain are not keyed at all.
        for flags in [&["-x", "c++"][..], &["-grecord-gcc-switches"], &["-fsanitize=undefined"], &["-O4"]] {
            let why = unit.why_not(compiler, flags, &lib);
            assert!(why.contains("the cache"), "{compiler} {flags:?}: {why}");
        }

        // Another program of the compiler's name, first on PATH through a
        // relative entry: that is what would run, and it is not a compiler
        // the cache can identify.
        let scratch = build.config.join("scratch");
        executable(&scratch.join(compiler), &format!("#!/bin/sh\nexec /usr/bin/env PATH=\"${{PATH#.:}}\" {compiler} -O3 \"$@\"\n"));
        let args = unit.args(&["-O2"], &lib);
        let path = format!(".:{}", std::env::var("PATH").unwrap());
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = build.wrap(compiler, &args).current_dir(&scratch).env("PATH", &path).output().unwrap();
        assert!(out.status.success(), "{}", text(&out.stderr));
        let last = build.events().pop().unwrap();
        assert!(last.contains("is a script, not a compiler cactup can identify") && !last.contains("\"key\""), "{last}");
        fs::remove_file(scratch.join(compiler)).unwrap();
    }
}

/// A stop signal while the key is being checked after the compile ends the
/// wrapper at once, as it would have ended a compiler still running.
#[test]
fn a_signal_during_the_check_after_the_compile_is_not_waited_out() {
    if !have("gcc") {
        return;
    }
    // In a serving build too, where the check stands between the compile
    // and publishing it: nothing may be published.
    for mode in ["record", "serve"] {
        signal_during_the_check(mode);
    }
}

fn signal_during_the_check(mode: &str) {
    // A check that never finishes: real GCC (only a compiler the cache
    // identifies gets a check), and a header that is a pipe with nobody
    // writing to it once the compile is through.
    let store = tempfile::tempdir().unwrap();
    let build = Build::new("record").serving(mode, store.path());
    let lib = tempfile::tempdir().unwrap();
    let lib = fs::canonicalize(lib.path()).unwrap();
    let unit = Unit::new(&build, "c", &lib);
    let fifo = lib.join("lib.h");
    fs::remove_file(&fifo).unwrap();
    assert!(Command::new("mkfifo").arg(&fifo).status().unwrap().success());
    // One write for each reader before the check: the key's preprocessor
    // run, the key's own read of the file, and the compile.
    let feed = |times: usize| {
        let fifo = fifo.clone();
        std::thread::spawn(move || {
            for _ in 0..times {
                fs::write(&fifo, "#define LIB_START 0\n").unwrap();
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        })
    };
    let feeder = feed(3);
    let args = unit.args(&["-O2"], &lib);
    let cwd = build.config.join("scratch");
    let mut child = build.wrap("gcc", &args.iter().map(String::as_str).collect::<Vec<_>>()).current_dir(&cwd).spawn().unwrap();
    feeder.join().unwrap();
    let waited = std::time::Instant::now();
    while !unit.object.exists() {
        assert!(waited.elapsed() < std::time::Duration::from_secs(20), "the compile never finished");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(child.try_wait().unwrap().is_none(), "the check should be waiting on the pipe");

    let asked = std::time::Instant::now();
    assert!(Command::new("kill").args(["-TERM", &child.id().to_string()]).status().unwrap().success());
    let status = child.wait().unwrap();
    assert_eq!(status.signal(), Some(15), "{status:?}");
    assert!(asked.elapsed() < std::time::Duration::from_secs(2));
    // Let go of the back end still waiting to open the pipe (not joined: if
    // nothing is waiting, neither is there anything to let go of).
    feed(1);
    std::thread::sleep(std::time::Duration::from_millis(100));
    let published: Vec<_> = walk(store.path()).into_iter().filter(|path| path.is_file()).collect();
    assert!(published.is_empty(), "{mode}: {published:?}");
}

/// Every path below `dir`.
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(walk(&path));
        }
        found.push(path);
    }
    found
}

/// Flags that reach a compiler from somewhere other than its command line
/// never pass the reader that decides what the cache can follow. A compiler
/// set up to take some — a Clang with a configuration file, a GCC with a
/// specs file, an override in the environment — is not keyed.
#[test]
fn a_compiler_given_flags_behind_its_command_line_is_not_keyed() {
    let compile = |build: &Build, compiler: &str, env: &[(&str, &str)]| {
        let dir = build.config.join("build/Thorn");
        fs::write(dir.join("plain.c"), "int plain;\n").unwrap();
        let out = build.wrap(compiler, &["-O2", "-c", "-o", "plain.o", "plain.c"]).current_dir(&dir).envs(env.iter().copied()).output().unwrap();
        assert!(out.status.success(), "{compiler}: {}", text(&out.stderr));
        build.events().pop().unwrap()
    };
    let found = |compiler: &str| {
        let out = Command::new("sh").args(["-c", &format!("command -v {compiler}")]).output().unwrap();
        fs::canonicalize(text(&out.stdout).trim()).unwrap()
    };

    // Clang reads `clang.cfg` beside its driver, unasked. (A copy of the
    // driver that cannot run where it is put — one that finds its libraries
    // relative to itself — leaves this part untried.)
    let bin = tempfile::tempdir().unwrap();
    let bin = fs::canonicalize(bin.path()).unwrap();
    let own = bin.join("clang").display().to_string();
    let copy_runs = have("clang")
        && fs::copy(found("clang"), bin.join("clang")).is_ok()
        && Command::new(&own).arg("--version").output().is_ok_and(|out| out.status.success());
    if copy_runs {
        let build = Build::new("record");
        assert!(compile(&build, &own, &[]).contains("\"key\""), "a copy of the driver is a compiler like the original");
        fs::write(bin.join("clang.cfg"), "-grecord-command-line\n").unwrap();
        // Every compile says for itself whether it reads one: no waiting
        // for the next attempt.
        let event = compile(&build, &own, &[]);
        assert!(event.contains("reads a configuration file") && !event.contains("\"key\""), "{event}");
        // The next attempt rules the compiler out as a whole, remembers
        // that, and forgets it when the file goes.
        let build = Build::new("record");
        assert!(compile(&build, &own, &[]).contains("reads a configuration file"));
        assert!(compile(&build, &own, &[]).contains("reads a configuration file"));
        // Turned off for one compile, the file is not read by that compile.
        assert!(compile(&build, &own, &[("CLANG_NO_DEFAULT_CONFIG", "1")]).contains("\"key\""));
        assert!(compile(&build, &own, &[]).contains("reads a configuration file"));
        fs::remove_file(bin.join("clang.cfg")).unwrap();
        assert!(compile(&build, &own, &[]).contains("\"key\""));

        // A configuration file for another target is read by the compiles
        // for that target only, and `clang --version` does not show it.
        let target = text(&Command::new(&own).args(["-m32", "--version"]).output().unwrap().stdout);
        if let Some(target) = target.lines().find_map(|line| line.strip_prefix("Target: ")) {
            fs::write(bin.join(format!("{target}-clang.cfg")), "-grecord-command-line\n").unwrap();
            let build = Build::new("record");
            assert!(compile(&build, &own, &[]).contains("\"key\""));
            let dir = build.config.join("build/Thorn");
            let out = build.wrap(&own, &["-m32", "-O2", "-c", "-o", "plain.o", "plain.c"]).current_dir(&dir).output().unwrap();
            assert!(out.status.success(), "{}", text(&out.stderr));
            let event = build.events().pop().unwrap();
            assert!(event.contains("reads a configuration file") && !event.contains("\"key\""), "{event}");
        }
    }
    if have("clang") {
        let build = Build::new("record");
        let event = compile(&build, "clang", &[("CCC_OVERRIDE_OPTIONS", "# +-grecord-command-line")]);
        assert!(event.contains("CCC_OVERRIDE_OPTIONS is set") && !event.contains("\"key\""), "{event}");
    }

    if have("gcc") {
        // GCC reads `specs` from the directories it finds its own programs
        // in; `GCC_EXEC_PREFIX` names another such directory (which then
        // has to have the compiler proper, too).
        let build = Build::new("record");
        let says = |question: &str| text(&Command::new("gcc").arg(question).output().unwrap().stdout).trim().to_owned();
        let version = Path::new(&says("-dumpmachine")).join(says("-dumpversion"));
        let prefix = build.root.join("own-gcc/lib/gcc");
        let (dir, programs) = (prefix.join(&version), build.root.join("own-gcc/libexec/gcc").join(&version));
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&programs).unwrap();
        std::os::unix::fs::symlink(says("-print-prog-name=cc1"), programs.join("cc1")).unwrap();
        let prefix = format!("{}/", prefix.display());
        assert!(compile(&build, "gcc", &[("GCC_EXEC_PREFIX", &prefix)]).contains("\"key\""));
        fs::write(dir.join("specs"), "*cc1:\n+ -O3\n\n").unwrap();
        // The compile says so itself, in the same attempt; and the next
        // attempt rules the compiler out as a whole.
        let event = compile(&build, "gcc", &[("GCC_EXEC_PREFIX", &prefix)]);
        assert!(event.contains("reads a specs file") && !event.contains("\"key\""), "{event}");
        let later = Build::new("record");
        let event = compile(&later, "gcc", &[("GCC_EXEC_PREFIX", &prefix)]);
        assert!(event.contains("reads a specs file") && !event.contains("\"key\""), "{event}");
    }
}

// ---------------------------------------------------------------------------
// Serving (spec §18.8).

impl Build {
    /// This build with `mode`, keeping its store at `store`.
    fn serving(self, mode: &str, store: &Path) -> Self {
        self.set_mode(mode, store);
        self
    }

    /// Change this build's mode and store.
    fn set_mode(&self, mode: &str, store: &Path) {
        let conf = fs::read_to_string(self.conf()).unwrap();
        let conf: String = conf
            .lines()
            .map(|line| match line.split_once(" = ").map(|(name, _)| name) {
                Some("mode") => format!("mode = \"{mode}\"\n"),
                Some("store") => format!("store = \"{}\"\n", store.display()),
                _ => format!("{line}\n"),
            })
            .collect();
        fs::write(self.conf(), conf).unwrap();
    }

    /// The last compile the log has, as JSON.
    fn last_event(&self) -> serde_json::Value {
        serde_json::from_str(self.events().last().expect("a compile was logged")).unwrap()
    }
}

impl Unit<'_> {
    /// Compile through the wrapper from the configuration's `scratch`, as
    /// Cactus does, and return what it printed.
    fn wrapped(&self, compiler: &str, flags: &[&str], lib: &Path) -> Output {
        let args = self.args(flags, lib);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let cwd = self.build.config.join("scratch");
        let out = self.build.wrap(compiler, &args).current_dir(&cwd).env("PWD", &cwd).output().unwrap();
        assert!(out.status.success(), "{compiler} {flags:?}: {}", text(&out.stderr));
        out
    }
}

/// A source that draws a warning naming its file, to see the messages a hit
/// replays.
const WARNED: &str = "static int never_used;\n";

/// A serving build publishes what it compiles, and serves it — to itself
/// and to another tree elsewhere under another configuration name — as the
/// object that tree's own compile, given the path map, would write. The
/// warning comes back naming the tree that is served, not the one that
/// compiled.
#[test]
fn a_serving_build_publishes_its_compiles_and_serves_them_elsewhere() {
    let shared = tempfile::tempdir().unwrap();
    let shared = fs::canonicalize(shared.path()).unwrap();
    let lib = shared.join("lib");
    fs::create_dir_all(&lib).unwrap();
    for compiler in ["gcc", "clang"] {
        if !have(compiler) {
            continue;
        }
        let store = shared.join(format!("store-{compiler}"));
        let one = Build::named("one", "sim", "record").serving("serve", &store);
        let other = Build::named("an/other/deeper", "renamed", "record").serving("serve", &store);
        let (here, there) = (Unit::new(&one, "c", &lib), Unit::new(&other, "c", &lib));
        for unit in [&here, &there] {
            let source = fs::read_to_string(&unit.source).unwrap();
            fs::write(&unit.source, format!("{source}{WARNED}")).unwrap();
        }
        let flags = ["-O2", "-g", "-Wall"];

        // A miss: compiled, given the map, and published.
        let first = here.wrapped(compiler, &flags, &lib);
        let event = one.last_event();
        assert_eq!((event["outcome"].as_str(), event["published"].as_bool()), (Some("miss"), Some(true)), "{compiler}: {event}");
        let compiled = fs::read(&here.object).unwrap();
        let warning = text(&first.stderr);
        assert!(warning.contains("never_used") && warning.contains(&one.root.display().to_string()), "{compiler}: {warning}");
        assert_eq!(here.mapped_object(compiler, &flags, &lib), compiled, "{compiler}: the miss was not compiled as the key says");

        // A hit: the same object, the same words, no compiler.
        fs::remove_file(&here.object).unwrap();
        let again = here.wrapped(compiler, &flags, &lib);
        let event = one.last_event();
        assert_eq!((event["outcome"].as_str(), event["compile_ms"].as_u64()), (Some("hit"), Some(0)), "{compiler}: {event}");
        assert_eq!(fs::read(&here.object).unwrap(), compiled, "{compiler}");
        assert_eq!((text(&again.stdout), text(&again.stderr)), (text(&first.stdout), warning.clone()), "{compiler}");

        // Elsewhere: served, as the object that tree would compile, with
        // the warning naming that tree.
        let elsewhere = there.wrapped(compiler, &flags, &lib);
        assert_eq!(other.last_event()["outcome"].as_str(), Some("hit"), "{compiler}: {}", other.last_event());
        let served = fs::read(&there.object).unwrap();
        assert_eq!(there.mapped_object(compiler, &flags, &lib), served, "{compiler}: served elsewhere, not that tree's object");
        let warning = text(&elsewhere.stderr);
        assert!(warning.contains(&other.root.display().to_string()), "{compiler}: {warning}");
        assert!(!warning.contains(&one.root.display().to_string()), "{compiler}: {warning}");
        assert!(!warning.contains("@CACTUP_"), "{compiler}: {warning}");
    }
}

/// A hit writes the dependency file the compile would have written, byte
/// for byte, with the target named or not; and leaves nothing else behind.
#[test]
fn a_hit_writes_the_dependency_file_the_compile_would_have() {
    let shared = tempfile::tempdir().unwrap();
    let shared = fs::canonicalize(shared.path()).unwrap();
    let lib = shared.join("lib");
    fs::create_dir_all(&lib).unwrap();
    for compiler in ["gcc", "clang"] {
        if !have(compiler) {
            continue;
        }
        let build = Build::new("record").serving("serve", &shared.join(format!("store-{compiler}")));
        let unit = Unit::new(&build, "c", &lib);
        let depfile = unit.object.with_extension("d");
        let depfile = depfile.to_str().unwrap();
        let object = unit.object.display().to_string();
        for target in [vec![], vec!["-MT", object.as_str()]] {
            let mut flags = vec!["-O2", "-MD", "-MP", "-MF", depfile];
            flags.extend(&target);
            let _ = fs::remove_file(&unit.object);
            unit.wrapped(compiler, &flags, &lib);
            let compiled = fs::read(depfile).unwrap();
            let mode = fs::metadata(depfile).unwrap().permissions().mode();
            fs::remove_file(&unit.object).unwrap();
            fs::remove_file(depfile).unwrap();
            unit.wrapped(compiler, &flags, &lib);
            assert_eq!(build.last_event()["outcome"].as_str(), Some("hit"), "{compiler} {target:?}");
            assert_eq!(text(&fs::read(depfile).unwrap()), text(&compiled), "{compiler} {target:?}");
            // Readable as the compiler leaves it: make skips a `.d` it cannot
            // read, and says nothing.
            assert_eq!(fs::metadata(depfile).unwrap().permissions().mode(), mode, "{compiler} {target:?}");
            let dir = unit.object.parent().unwrap();
            let strays: Vec<_> =
                fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name()).filter(|n| n.to_string_lossy().contains("cactup")).collect();
            assert!(strays.is_empty(), "{compiler}: {strays:?}");
            // The next round's compile is a miss again.
            let source = fs::read_to_string(&unit.source).unwrap();
            fs::write(&unit.source, format!("{source}/* round */\n")).unwrap();
        }
    }
}

/// Audit mode compiles a hit anyway: the same object is "same"; an entry
/// whose object is not what the compile makes (here made so by hand, with
/// a checksum to match) is "a wrong hit", and the build keeps the fresh
/// object.
#[test]
fn audit_mode_tells_a_wrong_hit() {
    if !have("gcc") {
        eprintln!("skipped: no GCC on this host");
        return;
    }
    let shared = tempfile::tempdir().unwrap();
    let shared = fs::canonicalize(shared.path()).unwrap();
    let (lib, store) = (shared.join("lib"), shared.join("store"));
    fs::create_dir_all(&lib).unwrap();
    let build = Build::new("record").serving("serve", &store);
    let unit = Unit::new(&build, "c", &lib);
    unit.wrapped("gcc", &["-O2"], &lib);
    let compiled = fs::read(&unit.object).unwrap();
    let key = build.last_event()["key"].as_str().unwrap().to_owned();
    build.set_mode("audit", &store);

    unit.wrapped("gcc", &["-O2"], &lib);
    let event = build.last_event();
    assert_eq!((event["outcome"].as_str(), event["audit"].as_str()), (Some("hit"), Some("same")), "{event}");

    // With a dependency file: the one the hit would have written is
    // compared with the compile's, and is the same.
    let depfile = unit.object.with_extension("d");
    let with_deps = ["-O2", "-MD", "-MP", "-MF", depfile.to_str().unwrap()];
    build.set_mode("serve", &store);
    unit.wrapped("gcc", &with_deps, &lib);
    build.set_mode("audit", &store);
    unit.wrapped("gcc", &with_deps, &lib);
    let event = build.last_event();
    assert_eq!((event["outcome"].as_str(), event["audit"].as_str()), (Some("hit"), Some("same")), "{event}");
    let strays: Vec<_> = fs::read_dir(unit.object.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|n| n.to_string_lossy().contains("cactup"))
        .collect();
    assert!(strays.is_empty(), "{strays:?}");

    corrupt_entry(&store, &key);
    unit.wrapped("gcc", &["-O2"], &lib);
    let event = build.last_event();
    assert_eq!(event["audit"].as_str(), Some("wrong-hit"), "{event}");
    assert_eq!(fs::read(&unit.object).unwrap(), compiled, "the build keeps the fresh object");

    // A second compile that does not end well (here it kills itself, as the
    // kernel kills a process out of memory) says nothing about the entry,
    // and costs the build nothing: the first compile's object stands.
    corrupt_entry(&store, &key);
    let killer = build.root.join("killed-compiler");
    executable(&killer, "#!/bin/sh\nkill -KILL $$\n");
    let args = unit.args(&["-O2"], &lib);
    let cwd = build.config.join("scratch");
    let out = build
        .wrap("gcc", &args.iter().map(String::as_str).collect::<Vec<_>>())
        .current_dir(&cwd)
        .env("PWD", &cwd)
        .env("CACTUP_CC_TEST_SECOND_COMPILER", &killer)
        .output()
        .unwrap();
    assert!(out.status.success(), "{:?} {}", out.status, text(&out.stderr));
    let event = build.last_event();
    assert_eq!(event["audit"].as_str(), Some("second-compile-failed"), "{event}");
    assert_eq!(fs::read(&unit.object).unwrap(), compiled, "the first compile's object is the build's");
    let strays: Vec<_> = fs::read_dir(unit.object.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|n| n.to_string_lossy().contains("cactup"))
        .collect();
    assert!(strays.is_empty(), "{strays:?}");
}

/// A compile that converts character sets keeps the locale in its key
/// whatever the compiler's locale trial said: GCC's `ASCII//TRANSLIT`
/// follows the locale (`cafe` in a UTF-8 one, `caf?` in C), and the trial
/// compiles without such flags.
#[test]
fn a_charset_conversion_keeps_the_locale_in_the_key() {
    if !have("gcc") {
        eprintln!("skipped: no GCC on this host");
        return;
    }
    let shared = tempfile::tempdir().unwrap();
    let shared = fs::canonicalize(shared.path()).unwrap();
    let build = Build::new("record").serving("serve", &shared.join("store"));
    let source = build.config.join("build/Thorn/translit.c");
    fs::write(&source, "const char *word = \"caf\u{e9}\";\n").unwrap();
    let object = build.config.join("build/Thorn/translit.c.o");
    let compile = |locale: &str| {
        let cwd = build.config.join("scratch");
        let args = ["-O2", "-fexec-charset=ASCII//TRANSLIT", "-c", "-o", object.to_str().unwrap(), source.to_str().unwrap()];
        let out = build.wrap("gcc", &args).current_dir(&cwd).env("PWD", &cwd).env("LC_ALL", locale).output().unwrap();
        assert!(out.status.success(), "{}", text(&out.stderr));
        (build.last_event(), fs::read(&object).unwrap())
    };
    // A host without the C.UTF-8 locale compiles both alike: nothing to show.
    let plain = |locale: &str| {
        let out = Command::new("gcc")
            .args(["-O2", "-fexec-charset=ASCII//TRANSLIT", "-c", "-o", "-", source.to_str().unwrap()])
            .env("LC_ALL", locale)
            .output()
            .unwrap();
        out.stdout
    };
    if plain("C.UTF-8") == plain("C") {
        eprintln!("skipped: the two locales compile alike on this host");
        return;
    }
    let (first, utf8) = compile("C.UTF-8");
    assert_eq!(first["published"].as_bool(), Some(true), "{first}");
    let (second, c) = compile("C");
    assert_eq!(second["outcome"].as_str(), Some("miss"), "the C session was served the UTF-8 session's object: {second}");
    assert_ne!(utf8, c, "the two locales were meant to compile differently here");
}

/// A compile whose output comes through the wrapper (a serving build's
/// miss) delivers every byte of it, also to a reader slower than the
/// compiler, and a failing one fails as it does without the cache.
#[test]
fn a_serving_compile_delivers_all_it_says_and_fails_as_it_would() {
    if !have("gcc") {
        eprintln!("skipped: no GCC on this host");
        return;
    }
    let shared = tempfile::tempdir().unwrap();
    let shared = fs::canonicalize(shared.path()).unwrap();
    let build = Build::new("record").serving("serve", &shared.join("store"));
    let source = build.config.join("build/Thorn/noisy.c");
    let noisy: String = (0..1500).map(|i| format!("static int unused_{i};\n")).collect();
    fs::write(&source, noisy).unwrap();
    let object = build.config.join("build/Thorn/noisy.c.o");
    let cwd = build.config.join("scratch");
    let args = ["-Wall", "-c", "-o", object.to_str().unwrap(), source.to_str().unwrap()];
    let plain = Command::new("gcc").args(args).current_dir(&cwd).output().unwrap();
    assert!(plain.stderr.len() > 100_000, "{}", plain.stderr.len());
    let mut child = build.wrap("gcc", &args).current_dir(&cwd).env("PWD", &cwd).stderr(Stdio::piped()).spawn().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let mut said = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        // A reader slower than the compiler: well past the wait for the
        // compiler's streams to close.
        std::thread::sleep(std::time::Duration::from_millis(150));
        match std::io::Read::read(&mut stderr, &mut buf).unwrap() {
            0 => break,
            n => said.extend_from_slice(&buf[..n]),
        }
    }
    assert!(child.wait().unwrap().success());
    assert_eq!(said.len(), plain.stderr.len(), "the wrapper lost some of what the compiler said");

    // A compile that fails: its status and its words, and nothing stored.
    fs::write(&source, "int broken(void) { return undeclared; }\n").unwrap();
    let failed = build.wrap("gcc", &args).current_dir(&cwd).env("PWD", &cwd).output().unwrap();
    let reference = Command::new("gcc").args(args).current_dir(&cwd).output().unwrap();
    assert_eq!(failed.status.code(), reference.status.code());
    assert_eq!(text(&failed.stderr), text(&reference.stderr));
    let event = build.last_event();
    assert_eq!((event["outcome"].as_str(), event["published"].as_bool()), (Some("miss"), None), "{event}");
}

/// With `relocate = false` (knob `build-cache-relocate no`), keys and
/// compiles keep the installation's paths: nothing is added to the compile,
/// and the object is a plain compile's.
#[test]
fn a_build_that_does_not_relocate_compiles_as_written() {
    if !have("gcc") {
        eprintln!("skipped: no GCC on this host");
        return;
    }
    let shared = tempfile::tempdir().unwrap();
    let shared = fs::canonicalize(shared.path()).unwrap();
    let lib = shared.join("lib");
    fs::create_dir_all(&lib).unwrap();
    let build = Build::new("record").serving("serve", &shared.join("store"));
    let conf = fs::read_to_string(build.conf()).unwrap().replace("relocate = true", "relocate = false");
    fs::write(build.conf(), conf).unwrap();
    let unit = Unit::new(&build, "c", &lib);
    unit.wrapped("gcc", &["-O2", "-g"], &lib);
    let event = build.last_event();
    assert_eq!((event["relocatable"].as_bool(), event["published"].as_bool()), (Some(false), Some(true)), "{event}");
    let served = fs::read(&unit.object).unwrap();
    let args = unit.args(&["-O2", "-g"], &lib);
    let cwd = build.config.join("scratch");
    assert!(Command::new("gcc").args(&args).current_dir(&cwd).env("PWD", &cwd).status().unwrap().success());
    assert_eq!(fs::read(&unit.object).unwrap(), served, "the compile was changed");
}

/// Change one byte of the object in the store's entry for `key`, with the
/// entry's checksum made to match: a wrong entry that looks whole.
fn corrupt_entry(store: &Path, key: &str) {
    let entry = store.join("v2/test").join(&key[..2]).join(key);
    let bytes = fs::read(&entry).unwrap();
    let line_end = bytes.iter().position(|b| *b == b'\n').unwrap() + 1;
    let lengths_end = line_end + bytes[line_end..].iter().position(|b| *b == b'\n').unwrap() + 1;
    let lengths: Vec<usize> = text(&bytes[line_end..lengths_end - 1]).split(' ').map(|n| n.parse().unwrap()).collect();
    let mut body = bytes[..bytes.len() - 65].to_vec();
    body[lengths_end + lengths[0] + lengths[1] / 2] ^= 0x55;
    let sum: String = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, &body).as_ref().iter().map(|b| format!("{b:02x}")).collect();
    body.extend(format!("{sum}\n").bytes());
    fs::remove_file(&entry).unwrap();
    fs::write(&entry, body).unwrap();
}

/// Does this host have a gfortran the cache keys?
fn have_gfortran() -> bool {
    let build = Build::new("record");
    fs::write(build.config.join("build/Thorn/probe.f90"), "subroutine probe()\nend subroutine\n").unwrap();
    let ran = build.wrap("gfortran", &["-c", "-o", "probe.o", "probe.f90"]).current_dir(build.config.join("build/Thorn")).output();
    let keyed = ran.is_ok_and(|out| out.status.success()) && build.events().first().is_some_and(|e| e.contains("\"key\""));
    if !keyed {
        eprintln!("skipped: no gfortran on this host that the cache keys: {:?}", build.events());
    }
    keyed
}

/// Two Cactus-shaped Fortran compiles (§18.10): a module that includes a
/// file beside its source and writes something (so that its object holds
/// the source's name for a runtime error message), its build copy beginning
/// with a line marker naming where it came from; and a source without one
/// that uses the module and draws a warning.
struct FortranUnits<'a> {
    build: &'a Build,
    provider: PathBuf,
    user: PathBuf,
}

impl<'a> FortranUnits<'a> {
    fn new(build: &'a Build) -> Self {
        let thorn = build.root.join("arrangements/Arr/Thorn/src");
        fs::create_dir_all(&thorn).unwrap();
        let dir = build.config.join("build/Thorn");
        fs::write(dir.join("vals.inc"), "integer, parameter :: seven = 7\n").unwrap();
        let provider = dir.join("provider.f90");
        fs::write(
            &provider,
            format!(
                "# 1 \"{}\"\nmodule provider\n  include 'vals.inc'\ncontains\n  subroutine say(u)\n    integer, intent(in) :: u\n    write (u, *) 'seven', seven\n  end subroutine\nend module provider\n",
                thorn.join("provider.F90").display()
            ),
        )
        .unwrap();
        let user = dir.join("user.f90");
        fs::write(&user, "module user\n  use provider\ncontains\n  subroutine twice(u)\n    integer, intent(in) :: u\n    integer :: unused\n    call say(u)\n    call say(u)\n  end subroutine\nend module user\n").unwrap();
        Self { build, provider, user }
    }

    fn object(source: &Path) -> PathBuf {
        source.with_extension("F90.o")
    }

    /// Compile `source` through the wrapper from `scratch`, as Cactus does.
    fn wrapped(&self, source: &Path) -> Output {
        let thorn = self.build.root.join("arrangements/Arr/Thorn/src");
        let object = Self::object(source);
        let args = ["-g", "-O2", "-Wall", "-fcray-pointer", "-ffixed-line-length-none"];
        let mut args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        args.push(format!("-I{}", thorn.display()));
        args.extend(["-c".to_owned(), "-o".to_owned(), object.display().to_string(), source.display().to_string()]);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let cwd = self.build.config.join("scratch");
        let out = self.build.wrap("gfortran", &args).current_dir(&cwd).env("PWD", &cwd).output().unwrap();
        assert!(out.status.success(), "{}: {}", source.display(), text(&out.stderr));
        out
    }

    fn module(&self, name: &str) -> PathBuf {
        self.build.config.join("scratch").join(name)
    }

    /// Whatever the cache left lying about in the build, the scratch and
    /// the attempt's directories.
    fn strays(&self) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for dir in [self.build.config.join("build/Thorn"), self.build.config.join("scratch"), self.build.cc.clone()] {
            for entry in fs::read_dir(&dir).unwrap() {
                let name = entry.unwrap().file_name().to_string_lossy().into_owned();
                if name.contains("cactup") || name.starts_with(".fortran-") || name.starts_with(".modules-") || name.starts_with(".module.") {
                    found.push(dir.join(name));
                }
            }
        }
        found
    }
}

/// A serving build publishes a Fortran compile with its module files and
/// serves it to another tree elsewhere under another configuration name, as
/// the object and the module files that tree's own compile writes. What the
/// compiler says names each tree's own files; a module file that would not
/// change is left alone; nothing is left lying about.
#[test]
fn fortran_is_served_with_its_module_files_elsewhere() {
    if !have_gfortran() {
        return;
    }
    let shared = tempfile::tempdir().unwrap();
    let shared = fs::canonicalize(shared.path()).unwrap();
    let store = shared.join("store");
    let one = Build::named("one", "sim", "record").serving("serve", &store);
    let other = Build::named("an/other/deeper", "renamed", "record").serving("serve", &store);
    let (here, there) = (FortranUnits::new(&one), FortranUnits::new(&other));

    // Misses: compiled, published with their module files.
    here.wrapped(&here.provider);
    let event = one.last_event();
    assert_eq!((event["outcome"].as_str(), event["published"].as_bool(), event["relocatable"].as_bool()), (Some("miss"), Some(true), Some(true)), "{event}");
    let first = here.wrapped(&here.user);
    assert_eq!(one.last_event()["published"].as_bool(), Some(true), "{}", one.last_event());
    let warning = text(&first.stderr);
    assert!(warning.contains("unused") && warning.contains(&here.user.display().to_string()), "the warning names this tree's file: {warning}");
    assert!(!warning.contains("cactup-root") && !warning.contains(".fortran-"), "{warning}");
    let objects: Vec<Vec<u8>> = [&here.provider, &here.user].iter().map(|source| fs::read(FortranUnits::object(source)).unwrap()).collect();
    for object in &objects {
        assert!(!text(object).contains(&one.root.display().to_string()), "the object names no real path of the tree");
    }
    let modules: Vec<Vec<u8>> = ["provider.mod", "user.mod"].iter().map(|name| fs::read(here.module(name)).unwrap()).collect();
    assert_eq!(here.strays(), Vec::<PathBuf>::new());

    // Hits elsewhere: the objects and the module files, and the warning
    // naming that tree.
    there.wrapped(&there.provider);
    assert_eq!(other.last_event()["outcome"].as_str(), Some("hit"), "{}", other.last_event());
    assert!(there.module("provider.mod").is_file(), "a hit puts the module file where the compile would");
    let served = there.wrapped(&there.user);
    assert_eq!(other.last_event()["outcome"].as_str(), Some("hit"), "{}", other.last_event());
    let warning = text(&served.stderr);
    assert!(warning.contains(&there.user.display().to_string()) && !warning.contains(&one.root.display().to_string()), "{warning}");
    assert!(!warning.contains("@CACTUP_") && !warning.contains("cactup-root"), "{warning}");
    let served_objects: Vec<Vec<u8>> = [&there.provider, &there.user].iter().map(|source| fs::read(FortranUnits::object(source)).unwrap()).collect();
    let served_modules: Vec<Vec<u8>> = ["provider.mod", "user.mod"].iter().map(|name| fs::read(there.module(name)).unwrap()).collect();
    assert_eq!((&served_objects, &served_modules), (&objects, &modules));

    // What that tree's own compiles write is what it was served.
    let control = shared.join("control-store");
    other.set_mode("serve", &control);
    for source in [&there.provider, &there.user] {
        fs::remove_file(FortranUnits::object(source)).unwrap();
        there.wrapped(source);
        assert_eq!(other.last_event()["outcome"].as_str(), Some("miss"));
    }
    let own: Vec<Vec<u8>> = [&there.provider, &there.user].iter().map(|source| fs::read(FortranUnits::object(source)).unwrap()).collect();
    assert_eq!(own, served_objects, "served elsewhere, not that tree's own object");
    let own_modules: Vec<Vec<u8>> = ["provider.mod", "user.mod"].iter().map(|name| fs::read(there.module(name)).unwrap()).collect();
    assert_eq!(own_modules, served_modules);

    // A module file that would not change is left alone, as gfortran
    // leaves it.
    other.set_mode("serve", &store);
    let before = fs::metadata(there.module("provider.mod")).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    there.wrapped(&there.provider);
    assert_eq!(other.last_event()["outcome"].as_str(), Some("hit"));
    assert_eq!(fs::metadata(there.module("provider.mod")).unwrap().modified().unwrap(), before);
    assert_eq!(there.strays(), Vec::<PathBuf>::new());
}

/// Audit mode compiles a Fortran hit anyway: the same object and module
/// files are "same"; an entry whose module file is not what the compile
/// writes is "a wrong hit", and the build keeps what it compiled.
#[test]
fn audit_mode_checks_fortran_module_files_too() {
    if !have_gfortran() {
        return;
    }
    let shared = tempfile::tempdir().unwrap();
    let store = fs::canonicalize(shared.path()).unwrap().join("store");
    let build = Build::new("record").serving("serve", &store);
    let units = FortranUnits::new(&build);
    units.wrapped(&units.provider);
    let key = build.last_event()["key"].as_str().unwrap().to_owned();
    let module = fs::read(units.module("provider.mod")).unwrap();
    build.set_mode("audit", &store);
    units.wrapped(&units.provider);
    let event = build.last_event();
    assert_eq!((event["outcome"].as_str(), event["audit"].as_str()), (Some("hit"), Some("same")), "{event}");

    // The module file in the entry, made wrong with a checksum to match.
    corrupt_entry_part(&store, &key, 4);
    units.wrapped(&units.provider);
    let event = build.last_event();
    assert_eq!(event["audit"].as_str(), Some("wrong-hit"), "{event}");
    assert_eq!(fs::read(units.module("provider.mod")).unwrap(), module, "the build keeps the module file it compiled");
    assert_eq!(units.strays(), Vec::<PathBuf>::new());
}

/// Record mode keys a Fortran compile and changes nothing about it: the
/// recipe's own command line runs.
#[test]
fn recording_fortran_compiles_as_written() {
    if !have_gfortran() {
        return;
    }
    let build = Build::new("record");
    let units = FortranUnits::new(&build);
    units.wrapped(&units.provider);
    let event = build.last_event();
    assert!(event["key"].is_string() && event["outcome"].is_null(), "{event}");
    let recorded = fs::read(FortranUnits::object(&units.provider)).unwrap();
    let cwd = build.config.join("scratch");
    let thorn = build.root.join("arrangements/Arr/Thorn/src");
    let object = FortranUnits::object(&units.provider);
    let status = Command::new("gfortran")
        .args(["-g", "-O2", "-Wall", "-fcray-pointer", "-ffixed-line-length-none"])
        .arg(format!("-I{}", thorn.display()))
        .arg("-c")
        .arg("-o")
        .arg(&object)
        .arg(&units.provider)
        .current_dir(&cwd)
        .env("PWD", &cwd)
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(fs::read(&object).unwrap(), recorded, "record mode changed the compile");
    assert_eq!(units.strays(), Vec::<PathBuf>::new());
}

/// What the cache cannot vouch for in a Fortran compile is passed through,
/// with its reason.
#[test]
fn fortran_the_cache_cannot_follow_is_passed_through() {
    if !have_gfortran() {
        return;
    }
    let build = Build::new("record");
    let dir = build.config.join("build/Thorn");
    let cwd = build.config.join("scratch");
    let elsewhere = build.config.join("modules");
    fs::create_dir_all(&elsewhere).unwrap();
    let elsewhere = elsewhere.display().to_string();
    for (file, content, flags, why) in [
        ("upper.F90", "module upper\nend module upper\n", vec![], "preprocesses"),
        ("comment.f90", "module comment\n  ! a /* in a comment\nend module comment\n", vec![], "/*"),
        ("moddir.f90", "module moddir\nend module moddir\n", vec!["-J", elsewhere.as_str()], "does not follow"),
    ] {
        fs::write(dir.join(file), content).unwrap();
        let mut args: Vec<String> = flags.iter().map(|f| (*f).to_owned()).collect();
        args.extend(["-c".to_owned(), "-o".to_owned(), dir.join(format!("{file}.o")).display().to_string(), dir.join(file).display().to_string()]);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = build.wrap("gfortran", &args).current_dir(&cwd).output().unwrap();
        assert!(out.status.success(), "{file}: {}", text(&out.stderr));
        let event = build.last_event();
        assert!(event["not_cached"].as_str().is_some_and(|reason| reason.contains(why)), "{file}: {event}");
    }
}

/// Change one byte of part `part` (0 the header, 1 the object, 2 stdout,
/// 3 stderr, 4 and on the module files) of the store's entry for `key`,
/// with the entry's checksum made to match.
fn corrupt_entry_part(store: &Path, key: &str, part: usize) {
    let entry = store.join("v2/test").join(&key[..2]).join(key);
    let bytes = fs::read(&entry).unwrap();
    let line_end = bytes.iter().position(|b| *b == b'\n').unwrap() + 1;
    let lengths_end = line_end + bytes[line_end..].iter().position(|b| *b == b'\n').unwrap() + 1;
    let lengths: Vec<usize> = text(&bytes[line_end..lengths_end - 1]).split(' ').map(|n| n.parse().unwrap()).collect();
    let start = lengths_end + lengths[..part].iter().sum::<usize>();
    let mut body = bytes[..bytes.len() - 65].to_vec();
    body[start + lengths[part] / 2] ^= 0x55;
    let sum: String = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, &body).as_ref().iter().map(|b| format!("{b:02x}")).collect();
    body.extend(format!("{sum}\n").bytes());
    fs::remove_file(&entry).unwrap();
    fs::write(&entry, body).unwrap();
}
