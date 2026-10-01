//! cactup as a compiler wrapper (§18.4): `CACTUP_CC_CMD='<compiler>' cactup
//! __cc <conf> <args…>`, the form the injected recipes run (`probe::inject_mk`).
//!
//! This is the one part of cactup that runs thousands of times per build
//! with a `make` recipe waiting on it, so it stays out of everything `main`
//! does for a command — no interrupt handler, no clap, no DB, no update
//! check (see [`run_if_invoked`], the first statement of `main`).
//!
//! **The compile always happens as `make` asked.** Whatever goes wrong in
//! here before the compiler has started ends in [`pass_through`], which
//! turns this process into the compiler; that includes a panic (the release
//! profile aborts on one, so [`install_panic_hook`] gets there first).
//! Once the compiler has run, its exit status is this process's exit status
//! and nothing after it can change that.

use super::probe::SELFTEST_COMPILER;
use super::{events_path, BuildConf, Mode, PROBE_VERB, WRAP_VERB};
use serde::Serialize;
use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicI32, AtomicU8, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

/// Carries what the recipe's compiler variable (`$(CC)`, `$(CXX)`, …)
/// expanded to, as one quoted value: the shell must not take it apart
/// before the wrapper has seen whether it is a plain command.
pub const CMD_ENV: &str = "CACTUP_CC_CMD";

/// Carries make's `$(SHELL)`, the shell the recipe itself runs under.
pub const SHELL_ENV: &str = "CACTUP_CC_SHELL";

/// Set to anything to have the wrapper say on stderr why it only passed a
/// compile through. Off by default: the wrapper's stderr is the compiler's.
const DEBUG_ENV: &str = "CACTUP_CC_DEBUG";

/// Tools that already stand in front of a compiler. cactup does not stack
/// on top of one: which of the two would see the real compiler, and what
/// the other would then key on, is not something to guess at.
const OTHER_WRAPPERS: &[&str] =
    &["ccache", "sccache", "distcc", "icecc", "icerun", "buildcache", "f90cache", "cachecc1"];

/// If this process was started as the compiler wrapper or as the probe, be
/// that and never return. Otherwise return at once, having touched nothing.
pub fn run_if_invoked() {
    let mut args = std::env::args_os().skip(1);
    match args.next() {
        Some(verb) if verb == WRAP_VERB => wrap(args.next(), args.collect()),
        Some(verb) if verb == PROBE_VERB => std::process::exit(super::probe::run(args.next())),
        _ => {}
    }
}

/// One compile as the recipe asked for it.
#[derive(Debug)]
struct Job {
    /// What the compiler variable expanded to for this target: usually
    /// `gcc`, possibly `nvcc --compiler-bindir /usr/bin/g++`, and in
    /// principle any shell text a makefile cares to put there.
    compiler: OsString,
    /// The rest of the recipe's command line.
    args: Vec<OsString>,
    /// The shell `make` runs this recipe's commands with.
    shell: PathBuf,
}

impl Job {
    /// The command line as words, if the compiler is a plain command: words
    /// the shell would have passed on exactly as they are, the first of
    /// them a program. Anything else (`LANG=C gcc`, `gcc -DX="a b"`, a
    /// pipeline) means what it means only to a shell, so it goes back to
    /// one (see [`pass_through`]) and the cache stays out of it.
    fn argv(&self) -> Option<Vec<OsString>> {
        let plain = |b: &u8| b.is_ascii_alphanumeric() || b"_-+./:,@%=~ \t".contains(b);
        let text = self.compiler.as_bytes();
        if !text.iter().all(plain) {
            return None;
        }
        let words: Vec<&[u8]> = text.split(|b| b.is_ascii_whitespace()).filter(|w| !w.is_empty()).collect();
        // The two characters let through above that a shell does read
        // something into, by position: `~` at the start of a word, and `=`
        // after a name at the start of the command.
        if words.iter().any(|word| word.starts_with(b"~")) || is_assignment(words.first()?) {
            return None;
        }
        let argv = words.into_iter().map(|w| OsString::from_vec(w.to_vec()));
        Some(argv.chain(self.args.iter().cloned()).collect())
    }
}

/// Is `word` a shell variable assignment (`NAME=value`)? A path with an `=`
/// in one of its directories is not: what precedes the `=` is not a name.
fn is_assignment(word: &[u8]) -> bool {
    let Some(name) = word.split(|b| *b == b'=').next().filter(|name| name.len() < word.len()) else {
        return false;
    };
    let start = |b: &u8| b.is_ascii_alphabetic() || *b == b'_';
    name.first().is_some_and(start) && name.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

// Where a compile stands, for the panic hook.
const BEFORE_COMPILE: u8 = 0;
const COMPILING: u8 = 1;
const AFTER_COMPILE: u8 = 2;
static PHASE: AtomicU8 = AtomicU8::new(BEFORE_COMPILE);
/// The exit status to leave with once the compiler has run.
static EXIT_CODE: AtomicI32 = AtomicI32::new(1);
/// The compile to fall back to before the compiler has run.
static JOB: OnceLock<Job> = OnceLock::new();
/// The running compiler's pid (0: none).
static CHILD: AtomicI32 = AtomicI32::new(0);
/// A stop signal that arrived before there was a compiler to pass it to.
static PENDING: AtomicI32 = AtomicI32::new(0);

/// Be the wrapper for one compile.
fn wrap(conf: Option<OsString>, args: Vec<OsString>) -> ! {
    install_panic_hook();
    let Some(compiler) = std::env::var_os(CMD_ENV).filter(|c| !c.is_empty()) else {
        eprintln!("cactup: the compiler wrapper was run without a compiler to wrap");
        std::process::exit(2);
    };
    let shell = std::env::var_os(SHELL_ENV).filter(|s| !s.is_empty()).map_or_else(|| "/bin/sh".into(), PathBuf::from);
    let job = JOB.get_or_init(|| Job { compiler, args, shell });
    #[cfg(debug_assertions)]
    test_panic("before");

    let conf_file = conf.map(PathBuf::from);
    let conf = conf_file.as_deref().map(BuildConf::load);
    // The build script's self-test (`probe::selftest_wrapped_mk`): no
    // compiler to run, only the question whether one could be wrapped here.
    if job.compiler == SELFTEST_COMPILER {
        match conf {
            Some(Ok(_)) => std::process::exit(0),
            Some(Err(e)) => eprintln!("cactup: {e:#}"),
            None => eprintln!("cactup: no configuration was named"),
        }
        std::process::exit(1);
    }
    let (Some(conf_file), Some(Ok(conf))) = (conf_file, conf) else {
        debug("its configuration cannot be read");
        pass_through(job)
    };
    let Some(argv) = job.argv() else {
        debug("the compiler is more than a plain command");
        pass_through(job)
    };
    let program = Path::new(&argv[0]).file_name().and_then(OsStr::to_str);
    if program.is_some_and(|p| OTHER_WRAPPERS.contains(&p)) {
        debug("the compiler already runs through another wrapper");
        pass_through(job)
    }
    match conf.mode {
        // `stage` writes no configuration for an off build; this is one
        // edited by hand.
        Mode::Off => pass_through(job),
        Mode::Record => {
            let started = Instant::now();
            let status = run(job, &argv);
            let event = Event {
                compiler: argv[0].to_string_lossy().into_owned(),
                exit: status.code(),
                signal: status.signal(),
                wall_ms: started.elapsed().as_millis() as u64,
            };
            if let Some(cc_dir) = conf_file.parent() {
                event.append(&events_path(cc_dir));
            }
            #[cfg(debug_assertions)]
            test_panic("after");
            leave_as(status)
        }
    }
}

/// `argv` as a command, in the environment the recipe had before the
/// fragment's two variables were added to it.
fn command(program: &OsStr, args: &[OsString]) -> Command {
    let mut command = Command::new(program);
    command.args(args).env_remove(CMD_ENV).env_remove(SHELL_ENV);
    command
}

/// Become the compiler: the same process, so the same stdin, signal
/// dispositions and make jobserver file descriptors, with nothing of cactup
/// left in between.
fn pass_through(job: &Job) -> ! {
    let Some(argv) = job.argv() else {
        // What `make` would have handed the shell: the compiler variable's
        // value as shell text (so its quotes and expansions mean what they
        // meant in the recipe), then the arguments, which `"$@"` passes on
        // untouched.
        let mut script = job.compiler.clone();
        script.push(" \"$@\"");
        let err = command(job.shell.as_os_str(), &[]).arg("-c").arg(script).arg(&job.shell).args(&job.args).exec();
        could_not_start(job.shell.as_os_str(), &err)
    };
    let err = command(&argv[0], &argv[1..]).exec();
    if let Some(script) = script_without_interpreter(&argv[0], &err) {
        let err = command(job.shell.as_os_str(), &argv[1..]).arg(&script).exec();
        could_not_start(script.as_os_str(), &err);
    }
    could_not_start(&argv[0], &err)
}

/// The compiler as a child process, for when the wrapper has work to do
/// after it.
fn spawn(job: &Job, argv: &[OsString]) -> std::io::Result<Child> {
    match command(&argv[0], &argv[1..]).spawn() {
        Err(err) => match script_without_interpreter(&argv[0], &err) {
            Some(script) => {
                let mut shell = command(job.shell.as_os_str(), &[]);
                shell.arg(script).args(&argv[1..]).spawn()
            }
            None => Err(err),
        },
        spawned => spawned,
    }
}

/// An executable text file with no `#!` line is a script to the shell that
/// runs a recipe (and to glibc's `execvp`), which answers the kernel's
/// `ENOEXEC` by interpreting the file itself. The kernel gives this process
/// the same error and no such help, so it must ask a shell. Returns the
/// file `program` names, if `err` is that case.
fn script_without_interpreter(program: &OsStr, err: &std::io::Error) -> Option<PathBuf> {
    if err.raw_os_error() != Some(rustix::io::Errno::NOEXEC.raw_os_error()) {
        return None;
    }
    if program.as_bytes().contains(&b'/') {
        return Some(PathBuf::from(program));
    }
    let executable = |path: &PathBuf| path.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    std::env::split_paths(&std::env::var_os("PATH")?).map(|dir| dir.join(program)).find(executable)
}

/// The compiler never ran: say so the way the shell would have, and leave
/// with the status the shell would have left with.
fn could_not_start(program: &OsStr, err: &std::io::Error) -> ! {
    eprintln!("cactup: {}: {err}", program.to_string_lossy());
    std::process::exit(if err.kind() == std::io::ErrorKind::NotFound { 127 } else { 126 });
}

/// The stop signals this process was started ignoring, from the `SigIgn`
/// mask in `/proc/self/status`.
fn ignored_signals() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let mask = status.lines().find_map(|line| line.strip_prefix("SigIgn:"))?;
    u64::from_str_radix(mask.trim(), 16).ok()
}

/// Runs in signal context: atomics and one `kill(2)`, nothing else.
fn pass_on(signal: i32) {
    let child = rustix::process::Pid::from_raw(CHILD.load(Ordering::SeqCst));
    if let (Some(child), Some(signal)) = (child, rustix::process::Signal::from_named_raw(signal)) {
        let _ = rustix::process::kill_process(child, signal);
    }
}

/// Run the compiler as a child and wait for it, passing on every signal
/// that asks this process to stop.
///
/// `make` signals the recipe it started, not that recipe's children, so a
/// wrapper that just died would leave the compiler running — and writing
/// the object `make` is about to delete as unfinished. Passing the signal
/// on and then ending as the compiler ends is what a bare compiler in the
/// recipe would have done.
///
/// A signal this process was started *ignoring* is left ignored, and so
/// reaches the compiler ignored, as it would have without the wrapper: a
/// build under `nohup` must survive the hangup. (A handler would not do: a
/// caught signal is back at its default action in the child.)
fn run(job: &Job, argv: &[OsString]) -> ExitStatus {
    let Some(ignored) = ignored_signals() else {
        debug("cannot tell which signals to leave ignored");
        pass_through(job)
    };
    for signal in [SIGHUP, SIGINT, SIGQUIT, SIGTERM] {
        if ignored & (1 << (signal - 1)) != 0 {
            continue;
        }
        let on_signal = move || {
            PENDING.store(signal, Ordering::SeqCst);
            pass_on(signal);
        };
        // SAFETY: the handler touches atomics and makes one raw `kill`
        // system call (rustix makes it without libc), all async-signal-safe.
        if unsafe { signal_hook::low_level::register(signal, on_signal) }.is_err() {
            debug("cannot watch for signals");
            pass_through(job)
        }
    }

    let mut child = match spawn(job, argv) {
        Ok(child) => child,
        Err(e) => could_not_start(&argv[0], &e),
    };
    CHILD.store(child.id() as i32, Ordering::SeqCst);
    PHASE.store(COMPILING, Ordering::SeqCst);
    // One that arrived after the handlers were in place and before the
    // compiler was: it would otherwise be lost, and the compile would run
    // on after `make` asked for it to stop.
    match PENDING.load(Ordering::SeqCst) {
        0 => {}
        signal => pass_on(signal),
    }

    match child.wait() {
        Ok(status) => {
            EXIT_CODE.store(exit_code(status), Ordering::SeqCst);
            PHASE.store(AFTER_COMPILE, Ordering::SeqCst);
            // The pid is free to be someone else's from here on.
            CHILD.store(0, Ordering::SeqCst);
            status
        }
        Err(e) => {
            eprintln!("cactup: lost track of the compiler it was wrapping ({}): {e}", argv[0].to_string_lossy());
            std::process::exit(1);
        }
    }
}

/// The exit status a shell reports for `status`.
fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

/// End as the compiler ended: with its exit code, or — for the signals that
/// ask a process to stop — killed by the signal that killed it, so `make`
/// sees what it would have seen without cactup. Any other fatal signal
/// (`SIGQUIT`, or a compiler that crashed) becomes the shell's `128 +
/// signal` exit code: dying of those here would leave a core file of
/// cactup, not of the compiler.
fn leave_as(status: ExitStatus) -> ! {
    if let Some(signal) = status.signal().filter(|s| [SIGHUP, SIGINT, SIGTERM].contains(s)) {
        // Resets the signal to its default action and raises it; returns
        // only if that did not end the process.
        let _ = signal_hook::low_level::emulate_default_handler(signal);
    }
    std::process::exit(exit_code(status));
}

/// A panic in the wrapper must not cost the build its compile.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|_| match PHASE.load(Ordering::SeqCst) {
        BEFORE_COMPILE => match JOB.get() {
            Some(job) => pass_through(job),
            // Not even the command line is known yet. Nothing this early
            // can panic; if it ever does, there is nothing to run.
            None => std::process::exit(70),
        },
        AFTER_COMPILE => std::process::exit(EXIT_CODE.load(Ordering::SeqCst)),
        // The compiler is running and nobody is waiting for it any more.
        _ => {
            let child = rustix::process::Pid::from_raw(CHILD.load(Ordering::SeqCst));
            let waited = rustix::process::waitpid(child, rustix::process::WaitOptions::empty());
            let code = match waited {
                Ok(Some((_, status))) => status
                    .exit_status()
                    .or_else(|| status.terminating_signal().map(|signal| 128 + signal))
                    .unwrap_or(1),
                _ => 1,
            };
            std::process::exit(code)
        }
    }));
}

/// A deliberate panic for the integration tests, in debug builds only.
#[cfg(debug_assertions)]
fn test_panic(at: &str) {
    if std::env::var_os("CACTUP_CC_TEST_PANIC").is_some_and(|v| v == at) {
        panic!("test panic {at} the compile");
    }
}

fn debug(why: &str) {
    if std::env::var_os(DEBUG_ENV).is_some() {
        eprintln!("cactup: passing this compile through: {why}");
    }
}

/// One line of `<attempt>/cc/events.jsonl`.
#[derive(Serialize)]
struct Event {
    compiler: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    signal: Option<i32>,
    wall_ms: u64,
}

impl Event {
    /// Best-effort: the log is for measurement, and a compile that ran is
    /// never failed over it. One `write` of one whole line in append mode,
    /// so the parallel compiles of a build do not interleave their lines.
    fn append(&self, path: &Path) {
        let Ok(mut line) = serde_json::to_string(self) else { return };
        line.push('\n');
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = file.write_all(line.as_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(compiler: &str, args: &[&str]) -> Job {
        Job {
            compiler: OsString::from(compiler),
            args: args.iter().map(OsString::from).collect(),
            shell: PathBuf::from("/bin/sh"),
        }
    }

    fn argv(compiler: &str, args: &[&str]) -> Option<Vec<String>> {
        let argv = job(compiler, args).argv()?;
        Some(argv.into_iter().map(|w| w.into_string().unwrap()).collect())
    }

    #[test]
    fn a_plain_compiler_is_its_words_then_the_arguments() {
        assert_eq!(argv("gcc", &["-c", "a b.c"]).unwrap(), ["gcc", "-c", "a b.c"]);
        assert_eq!(
            argv(" nvcc\t--compiler-bindir  /usr/bin/g++ ", &["-c"]).unwrap(),
            ["nvcc", "--compiler-bindir", "/usr/bin/g++", "-c"]
        );
        // A flag carrying `=` is fine, and so is a directory with one: only
        // a name followed by `=` at the start is an assignment.
        assert_eq!(argv("hipcc --amdgpu-target=gfx90a", &[]).unwrap(), ["hipcc", "--amdgpu-target=gfx90a"]);
        assert_eq!(argv("/opt/gcc=13/a~b/bin/gcc", &[]).unwrap(), ["/opt/gcc=13/a~b/bin/gcc"]);
        assert_eq!(argv("./cc=x", &[]).unwrap(), ["./cc=x"]);
        // The arguments are the shell's words already: nothing in them is
        // looked at.
        assert_eq!(argv("gcc", &["-DX=\"a b\"", "$HOME"]).unwrap(), ["gcc", "-DX=\"a b\"", "$HOME"]);
    }

    #[test]
    fn anything_a_shell_would_interpret_is_not_plain() {
        for compiler in [
            "LANG=C gcc",
            "_X9=1 gcc",
            "CC=",
            "gcc ~/x.o",
            "gcc -DNAME=\"a b\"",
            "gcc -DNAME='a'",
            "$HOME/bin/gcc",
            "~/bin/gcc",
            "cd /x && gcc",
            "gcc; true",
            "gcc | tee log",
            "gcc > log",
            "gcc *.c",
            "`which gcc`",
            "gcc \\",
            "gcc\n-c",
            " ",
            "",
        ] {
            assert_eq!(argv(compiler, &["-c"]), None, "{compiler:?}");
        }
    }
}
