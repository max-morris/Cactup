//! cactup as a compiler wrapper (§18.4): `cactup __cc <conf> <VAR>:<n>
//! <compiler…> <args…>`, or `cactup-cc <compiler> <args…>` by file name.
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

use super::{events_path, BuildConf, Mode, CONF_ENV, PROBE_VERB, WRAP_ARGV0, WRAP_VERB};
use serde::Serialize;
use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
use signal_hook::iterator::Signals;
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

/// Set to anything to have the wrapper say on stderr why it left the cache
/// out of a compile. Off by default: the wrapper's stderr is the compiler's.
const DEBUG_ENV: &str = "CACTUP_CC_DEBUG";

/// If this process was started as the compiler wrapper or as the probe, be
/// that and never return. Otherwise return at once, having touched nothing.
pub fn run_if_invoked() {
    let mut args = std::env::args_os();
    let Some(argv0) = args.next() else { return };
    // By exact name only: a versioned binary is `cactup-<build id>`, and a
    // build id may well begin with "cc".
    if Path::new(&argv0).file_name() == Some(OsStr::new(WRAP_ARGV0)) {
        let command = Compiler::Given(args.collect());
        wrap(std::env::var_os(CONF_ENV), command);
    }
    match args.next() {
        Some(verb) if verb == WRAP_VERB => {
            let conf = args.next();
            let rest: Vec<OsString> = args.collect();
            wrap(conf, Compiler::from_marked(rest));
        }
        Some(verb) if verb == PROBE_VERB => std::process::exit(super::probe::run(args.next())),
        _ => {}
    }
}

/// The command line to run, and how the wrapper came by it.
#[derive(Debug, PartialEq)]
enum Compiler {
    /// The command line as given: the wrapper stands in front of exactly it.
    Given(Vec<OsString>),
    /// The make variable (`CC`, `CXX`, …) holds another command than the one
    /// the probe wrapped — a thorn's `make.code.defn` reassigned it for its
    /// own sources. The variable's value is what `make` would have run, so
    /// it runs, through the shell as `make` would have, and the cache stays
    /// out of it.
    Reassigned { var: String, value: OsString, args: Vec<OsString> },
}

impl Compiler {
    /// Parse `<VAR>:<n> <n words of the configured command> <args…>`, the
    /// form the injection fragment writes (`probe::inject_mk`).
    ///
    /// The fragment sets `<VAR>` for object targets only and `private`, so
    /// the recipe's *environment* still carries the variable as the
    /// makefiles last set it globally (the probe's self-test checks that
    /// this `make` behaves so). When that differs from the words the probe
    /// read out of `make.config.defn`, the makefiles changed the compiler
    /// after the fragment was read, and the wrapper must not undo that.
    fn from_marked(mut rest: Vec<OsString>) -> Self {
        let Some((var, count)) = rest.first().and_then(|m| parse_marker(m)) else {
            return Self::Given(rest);
        };
        if rest.len() < 1 + count {
            return Self::Given(rest.split_off(1));
        }
        let args = rest.split_off(1 + count);
        let configured = rest.split_off(1);
        let current = std::env::var_os(&var);
        match reassigned(&configured, current.as_deref()) {
            Some(value) => Self::Reassigned { var, value: value.to_owned(), args },
            None => Self::Given(configured.into_iter().chain(args).collect()),
        }
    }
}

/// `CC:1` → `("CC", 1)`.
fn parse_marker(marker: &OsStr) -> Option<(String, usize)> {
    let (var, count) = marker.to_str()?.split_once(':')?;
    let named = !var.is_empty() && var.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
    let count: usize = count.parse().ok()?;
    (named && count > 0).then(|| (var.to_owned(), count))
}

/// The environment's value of the compiler variable, if it names another
/// command than the `configured` words.
fn reassigned<'a>(configured: &[OsString], current: Option<&'a OsStr>) -> Option<&'a OsStr> {
    let current = current?;
    let bytes = current.as_bytes();
    let words: Vec<&[u8]> =
        bytes.split(|b| b.is_ascii_whitespace()).filter(|w| !w.is_empty()).collect();
    let same = words.len() == configured.len()
        && words.iter().zip(configured).all(|(w, c)| *w == c.as_bytes());
    // An empty value, or one that is this very wrapper's command line (a
    // make that exports the pattern-specific value after all), says nothing
    // about what the makefiles wanted.
    let wrapped = words.iter().any(|w| *w == WRAP_VERB.as_bytes());
    (!same && !words.is_empty() && !wrapped).then_some(current)
}

// Where a compile stands, for the panic hook.
const BEFORE_COMPILE: u8 = 0;
const COMPILING: u8 = 1;
const AFTER_COMPILE: u8 = 2;
static PHASE: AtomicU8 = AtomicU8::new(BEFORE_COMPILE);
/// The exit status to leave with once the compiler has run.
static EXIT_CODE: AtomicI32 = AtomicI32::new(1);
/// The command line to fall back to before the compiler has run.
static FALLBACK: OnceLock<Compiler> = OnceLock::new();
/// The running compiler's pid (0: none yet).
static CHILD: AtomicI32 = AtomicI32::new(0);
/// The name of the thread that forwards signals to the compiler.
const FORWARDER: &str = "cactup-cc-signals";

/// Be the wrapper for one compile.
fn wrap(conf: Option<OsString>, compiler: Compiler) -> ! {
    if matches!(&compiler, Compiler::Given(argv) if argv.is_empty()) {
        eprintln!("cactup: the compiler wrapper was run without a compiler to wrap");
        std::process::exit(2);
    }
    let compiler = FALLBACK.get_or_init(|| compiler);
    install_panic_hook();
    #[cfg(debug_assertions)]
    test_panic("before");

    let Some(conf_file) = conf else {
        debug("no configuration was named");
        pass_through(compiler)
    };
    let conf_file = Path::new(&conf_file);
    let conf = match BuildConf::load(conf_file) {
        Ok(conf) => conf,
        Err(e) => {
            debug(&format!("{e:#}"));
            pass_through(compiler)
        }
    };
    let Compiler::Given(argv) = compiler else {
        debug("the compiler variable was reassigned after the configuration was read");
        pass_through(compiler)
    };
    match conf.mode {
        Mode::Off => pass_through(compiler),
        Mode::Record => {
            let started = Instant::now();
            let status = run(argv);
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

/// Become the compiler: the same process, so the same stdin, signals, and
/// make jobserver file descriptors, with nothing of cactup left in between.
fn pass_through(compiler: &Compiler) -> ! {
    let (program, err) = match compiler {
        Compiler::Given(argv) => (argv[0].as_os_str(), Command::new(&argv[0]).args(&argv[1..]).exec()),
        Compiler::Reassigned { value, args, .. } => {
            // What `make` would have handed the shell: the variable's value
            // as shell text (so its quotes and expansions mean what they
            // meant to `make`'s recipe), then the arguments, which `"$@"`
            // passes on untouched.
            let mut script = value.clone();
            script.push(" \"$@\"");
            let err = Command::new("/bin/sh").arg("-c").arg(script).arg("sh").args(args).exec();
            (OsStr::new("/bin/sh"), err)
        }
    };
    could_not_start(program, &err)
}

/// The compiler never ran: say so the way the shell would have, and leave
/// with the status the shell would have left with.
fn could_not_start(program: &OsStr, err: &std::io::Error) -> ! {
    eprintln!("cactup: {}: {err}", program.to_string_lossy());
    std::process::exit(if err.kind() == std::io::ErrorKind::NotFound { 127 } else { 126 });
}

/// Run the compiler as a child and wait for it, passing on every signal
/// that asks this process to stop.
///
/// `make` signals the recipe it started, not that recipe's children, so a
/// wrapper that just died would leave the compiler running — and writing
/// the object `make` is about to delete as unfinished. Forwarding the
/// signal and then dying of it, as the compiler does, is what a bare
/// compiler in the recipe would have done.
fn run(argv: &[OsString]) -> ExitStatus {
    // Registered before the child exists, so no signal can arrive in
    // between and kill only the wrapper.
    let Ok(mut signals) = Signals::new([SIGHUP, SIGINT, SIGQUIT, SIGTERM]) else {
        debug("cannot watch for signals");
        pass_through(&Compiler::Given(argv.to_vec()))
    };
    let mut child = match Command::new(&argv[0]).args(&argv[1..]).spawn() {
        Ok(child) => child,
        Err(e) => could_not_start(&argv[0], &e),
    };
    let pid = rustix::process::Pid::from_child(&child);
    CHILD.store(pid.as_raw_nonzero().get(), Ordering::SeqCst);
    PHASE.store(COMPILING, Ordering::SeqCst);

    let reaped = Arc::new(AtomicBool::new(false));
    let handle = signals.handle();
    let forwarder = {
        let reaped = Arc::clone(&reaped);
        // `Builder::spawn`, not `thread::spawn`: a process at its thread
        // limit gets an error here instead of a panic, and the compile
        // carries on without forwarding.
        std::thread::Builder::new().name(FORWARDER.to_owned()).spawn(move || {
            for signal in signals.forever() {
                // Never after the child is reaped: its pid is free to be
                // someone else's by then.
                if reaped.load(Ordering::SeqCst) {
                    break;
                }
                if let Some(signal) = rustix::process::Signal::from_named_raw(signal) {
                    let _ = rustix::process::kill_process(pid, signal);
                }
            }
        })
    };
    let status = child.wait();
    reaped.store(true, Ordering::SeqCst);
    handle.close();
    if let Ok(forwarder) = forwarder {
        let _ = forwarder.join();
    }

    match status {
        Ok(status) => {
            EXIT_CODE.store(exit_code(status), Ordering::SeqCst);
            PHASE.store(AFTER_COMPILE, Ordering::SeqCst);
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
/// (a compiler that crashed) becomes the shell's `128 + signal` exit code:
/// dying of SIGSEGV here would leave a core file of cactup, not of the
/// compiler.
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
        BEFORE_COMPILE => match FALLBACK.get() {
            Some(compiler) => pass_through(compiler),
            None => std::process::exit(70),
        },
        AFTER_COMPILE => std::process::exit(EXIT_CODE.load(Ordering::SeqCst)),
        // The compiler is running. On the signal thread, stay out of the
        // way: the main thread is waiting for the compiler and finishes the
        // job. On the main thread, do that waiting here.
        _ if std::thread::current().name() == Some(FORWARDER) => loop {
            std::thread::park();
        },
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
        eprintln!("cactup-cc: not caching this compile: {why}");
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

    fn os(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    #[test]
    fn markers() {
        assert_eq!(parse_marker(OsStr::new("CC:1")), Some(("CC".to_owned(), 1)));
        assert_eq!(parse_marker(OsStr::new("F90:3")), Some(("F90".to_owned(), 3)));
        for bad in ["gcc", "CC:", "CC:0", "cc:1", ":1", "CC:x", "/usr/bin/gcc", "C:/x"] {
            assert_eq!(parse_marker(OsStr::new(bad)), None, "{bad}");
        }
    }

    #[test]
    fn an_unchanged_variable_is_not_a_reassignment() {
        let configured = os(&["nvcc", "--compiler-bindir", "/usr/bin/g++"]);
        for same in ["nvcc --compiler-bindir /usr/bin/g++", "  nvcc\t--compiler-bindir  /usr/bin/g++ "] {
            assert_eq!(reassigned(&configured, Some(OsStr::new(same))), None, "{same}");
        }
        // Unset or empty: nothing to go by, so the configured words stand.
        assert_eq!(reassigned(&configured, None), None);
        assert_eq!(reassigned(&configured, Some(OsStr::new(" "))), None);
        // A make that exports the wrapped value itself.
        let own = "'/opt/cactup-abc' __cc '/b/cc/config.toml' CUCC:3 nvcc --compiler-bindir /usr/bin/g++";
        assert_eq!(reassigned(&configured, Some(OsStr::new(own))), None);
    }

    #[test]
    fn a_changed_variable_is_a_reassignment() {
        let configured = os(&["gcc"]);
        assert_eq!(reassigned(&configured, Some(OsStr::new("clang"))), Some(OsStr::new("clang")));
        assert_eq!(reassigned(&configured, Some(OsStr::new("gcc -m32"))), Some(OsStr::new("gcc -m32")));
    }

    #[test]
    fn the_marked_form_splits_command_from_arguments() {
        // SAFETY: test-only; no other test reads this variable.
        unsafe { std::env::remove_var("CACTUP_TEST_UNSET_CC") };
        let given = Compiler::from_marked(os(&["CACTUP_TEST_UNSET_CC:2", "nvcc", "-x", "-c", "a.cu"]));
        assert_eq!(given, Compiler::Given(os(&["nvcc", "-x", "-c", "a.cu"])));

        // No marker (the by-name form, or anything unexpected): the words
        // are the command line.
        let bare = Compiler::from_marked(os(&["gcc", "-c", "a.c"]));
        assert_eq!(bare, Compiler::Given(os(&["gcc", "-c", "a.c"])));

        // A marker promising more words than there are is dropped, and what
        // follows it runs as is.
        let short = Compiler::from_marked(os(&["CC:5", "gcc", "-c"]));
        assert_eq!(short, Compiler::Given(os(&["gcc", "-c"])));
    }
}
