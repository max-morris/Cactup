//! cactup as a compiler wrapper (§18.4): `cactup __cc <conf> <compiler>
//! <shell> <args…>`, the form the injected recipes run (`probe::inject_mk`).
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
//!
//! **The recipe's shell is the reference.** Without cactup, the shell that
//! runs the recipe decides what the compiler text means and how to start
//! it. So whatever this process cannot start itself — a text that is more
//! than plain words, a shell keyword or function, a script without an
//! interpreter line — it hands to that same kind of shell
//! ([`hand_to_shell`]) rather than fail a compile the recipe would have run.

use super::event::{Audit, Event, Outcome};
use super::store::{About, Miss, ModulesTo, NewEntry, Published, Store};
use super::probe::SELFTEST_COMPILER;
use super::{events_path, key, BuildConf, Mode, PROBE_VERB, WRAP_VERB};
use crate::Res;
use anyhow::Context;
use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::io::{Read, Write};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicI32, AtomicU8, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

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
        Some(verb) if verb == WRAP_VERB => wrap(args),
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
    /// them naming a program the shell would have found on `PATH` or by its
    /// path. Anything else (`LANG=C gcc`, `gcc -DX="a b"`, a pipeline, a
    /// name the shell has a meaning of its own for) means what it means
    /// only to a shell, so it goes back to one (see [`pass_through`]) and
    /// the cache stays out of it.
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
        if shell_resolves_differently(OsStr::from_bytes(words[0])) {
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

/// Words a POSIX shell or bash gives a meaning before it looks for a
/// program: reserved words and builtins. `time gcc` is bash's keyword even
/// where `/usr/bin/time` exists.
const SHELL_WORDS: &[&str] = &[
    "!", ".", ":", "[", "[[", "alias", "bg", "break", "builtin", "caller", "case", "cd", "command", "compgen",
    "complete", "compopt", "continue", "coproc", "declare", "dirs", "disown", "do", "done", "echo", "elif", "else",
    "bind", "chdir", "enable", "esac", "eval", "exec", "exit", "export", "false", "fc", "fg", "fi", "for", "function",
    "getopts",
    "hash", "help", "history", "if", "in", "jobs", "kill", "let", "local", "logout", "mapfile", "popd", "printf",
    "pushd", "pwd", "read", "readarray", "readonly", "return", "select", "set", "shift", "shopt", "source",
    "suspend", "test", "then", "time", "times", "trap", "true", "type", "typeset", "ulimit", "umask", "unalias",
    "unset", "until", "wait", "while", "{", "}",
];

/// Would the recipe's shell run something else for the command name
/// `program` than the program a `PATH` search finds? It would for a name
/// the shell itself gives a meaning ([`SHELL_WORDS`]), for a function
/// exported to it under that name (bash's `export -f`), and it might when
/// `PATH` has an entry beginning with `~` ahead of the directory that has
/// the program: bash expands such an entry as it searches, other shells
/// and this process do not. A name with a `/` in it is a path to all of
/// them.
///
/// What this cannot see is a function or alias the shell defines for
/// itself at startup (bash reads the file `BASH_ENV` names). Such a file
/// would have to define one named like the compiler.
fn shell_resolves_differently(program: &OsStr) -> bool {
    let name = program.as_bytes();
    if name.contains(&b'/') {
        return false;
    }
    if program.to_str().is_some_and(|name| SHELL_WORDS.contains(&name)) {
        return true;
    }
    // bash exports a function as `BASH_FUNC_<name>%%` (`BASH_FUNC_<name>()`
    // in some patched 4.x builds); before that, as a variable of the same
    // name whose value begins `() {`.
    let exported_function = |(var, value): (OsString, OsString)| {
        let var = var.as_bytes();
        match var.strip_prefix(b"BASH_FUNC_") {
            Some(rest) => rest.strip_suffix(b"%%").or_else(|| rest.strip_suffix(b"()")) == Some(name),
            None => var == name && value.as_bytes().starts_with(b"() {"),
        }
    };
    if std::env::vars_os().any(exported_function) {
        return true;
    }
    // A `~` entry matters only if the shell reaches it: that is, if it
    // comes before the directory the program is found in.
    let path = std::env::var_os("PATH").unwrap_or_default();
    let has_program = |dir: &[u8]| Path::new(OsStr::from_bytes(dir)).join(program).is_file();
    let reached = path.as_bytes().split(|b| *b == b':').take_while(|dir| !has_program(dir));
    reached.into_iter().any(|entry| entry.starts_with(b"~"))
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

/// Be the wrapper for one compile: `<conf> <compiler> <shell> <args…>`.
fn wrap(mut args: impl Iterator<Item = OsString>) -> ! {
    install_panic_hook();
    let (Some(conf_file), Some(compiler), Some(shell)) = (args.next(), args.next(), args.next()) else {
        eprintln!("cactup: the compiler wrapper was run without a compiler to wrap");
        std::process::exit(2);
    };
    let shell = if shell.is_empty() { PathBuf::from("/bin/sh") } else { PathBuf::from(shell) };
    let job = JOB.get_or_init(|| Job { compiler, args: args.collect(), shell });
    #[cfg(debug_assertions)]
    test_panic("before");

    let conf_file = Path::new(&conf_file);
    let conf = BuildConf::load(conf_file);
    // The build script's self-test (`probe::selftest_wrapped_mk`): no
    // compiler to run, only the question whether one could be wrapped here.
    if job.compiler == SELFTEST_COMPILER {
        let ready = conf.and_then(|_| ignored_signals());
        if let Err(e) = &ready {
            eprintln!("cactup: {e:#}");
        }
        std::process::exit(if ready.is_ok() { 0 } else { 1 });
    }
    let Ok(conf) = conf else {
        debug("its configuration cannot be read");
        pass_through(job)
    };
    let cc_dir = conf_file.parent().unwrap_or(Path::new("."));
    let Some(argv) = job.argv() else {
        leave_to(job, &conf, cc_dir, "the compiler is not a plain command naming a program, so the recipe's shell runs it")
    };
    // The compiler is what the recipe's shell makes of its name, and that
    // shell may have defined the name for itself at startup: it is asked
    // (§18.4).
    if conf.mode != Mode::Off
        && let Err(why) = super::lookup::shell_runs_program(cc_dir, &job.shell, &argv[0])
    {
        leave_to_shell(job, &conf, cc_dir, &format!("{why}, so the recipe's shell runs it"))
    }
    let program = Path::new(&argv[0]).file_name().and_then(OsStr::to_str);
    if program.is_some_and(|p| OTHER_WRAPPERS.contains(&p)) {
        leave_to(job, &conf, cc_dir, "the compiler already runs through another wrapper")
    }
    match conf.mode {
        // `stage` writes no configuration for an off build; this is one
        // edited by hand.
        Mode::Off => pass_through(job),
        mode => cached(job, &conf, cc_dir, &argv, mode),
    }
}

/// The most of a compiler's stdout, and of its stderr, a serving cache
/// keeps to store with the object (§18.8). A compile that says more is
/// passed on whole and not published.
const MESSAGES_CAP: usize = 4 << 20;

/// One compile through the cache: key it; in a serving build, serve it from
/// the store if it can (§18.8); else run it, check that the key still
/// describes what was compiled, and in a serving build publish the result.
/// Record mode does all of it but the serving and the publishing, and adds
/// nothing to the compile.
fn cached(job: &Job, conf: &BuildConf, cc_dir: &Path, argv: &[OsString], mode: Mode) -> ! {
    let store = match mode.serves() {
        true => Store::new(&conf.store, &conf.machine).map_err(|e| format!("{e:#}")),
        false => Err(String::new()),
    };
    let (keyed, key_ms) = timed(|| key::key(conf, cc_dir, argv, store.is_ok()));
    let mut keyed = keyed;
    let output = match &keyed {
        Ok(keyed) => Some(keyed.compile.output.clone()),
        Err(_) => output_of(&argv[1..]),
    };
    let mut event = Event {
        compiler: argv[0].to_string_lossy().into_owned(),
        unit: output.as_deref().and_then(|output| unit(conf, output)),
        key: keyed.as_ref().ok().map(|keyed| keyed.parts.key()),
        parts: keyed.as_ref().ok().map(|keyed| keyed.parts.clone()),
        not_cached: keyed.as_ref().err().cloned(),
        relocatable: keyed.as_ref().ok().map(key::Keyed::relocatable),
        text_bytes: keyed.as_ref().ok().map(|keyed| keyed.text_bytes),
        files: keyed.as_ref().ok().map(|keyed| keyed.files as u64),
        key_ms,
        ..Default::default()
    };
    if let Err(why) = &store
        && mode.serves()
    {
        event.store = Some(why.clone());
    }
    // The store, for a compile it can take part in.
    let serving = match (&store, &mut keyed) {
        (Ok(store), Ok(keyed)) => Some((store, keyed)),
        _ => None,
    };

    // A hit (§18.8). In audit mode, the stored object goes beside the
    // object, and its module files beside theirs, to be compared with the
    // ones the compile is about to write.
    let mut audited: Option<(tempfile::TempPath, Vec<(String, tempfile::TempPath)>)> = None;
    // Module files go where the compile writes them: its working directory
    // (§18.10).
    let cwd = std::env::current_dir().unwrap_or_default();
    if let Some((store, keyed)) = serving {
        let key = keyed.parts.key();
        event.outcome = Some(Outcome::Miss);
        // In audit mode, an entry that cannot be put beside the object
        // cannot be checked: then the compile just runs.
        let into = match mode {
            Mode::Audit => beside(&keyed.compile.output).map(Some).map_err(|e| e.to_string()),
            _ => Ok(None),
        };
        let modules_to = match (keyed.compile.language.is_fortran(), mode) {
            (false, _) => ModulesTo::Nowhere,
            (true, Mode::Audit) => ModulesTo::Aside(&cwd),
            (true, _) => ModulesTo::Into(&cwd),
        };
        let (restored, serve_ms) = timed(|| match &into {
            Ok(into) => store.restore_with(&key, into.as_deref().unwrap_or(&keyed.compile.output), modules_to),
            Err(why) => Err(Miss::CannotWrite(why.clone())),
        });
        let into = into.ok().flatten();
        event.serve_ms = serve_ms;
        match restored {
            Ok(restored) if mode == Mode::Audit => {
                event.outcome = Some(Outcome::Hit);
                audited = into.map(|object| (object, restored.aside));
            }
            // The object is in place; with the dependency file, the compile
            // is done. Without it, it runs after all, and writes both.
            Ok(restored) => match keyed.keep_depend() {
                Ok(()) => {
                    let messages = restored.messages;
                    event.outcome = Some(Outcome::Hit);
                    event.exit = Some(0);
                    event.object_bytes = std::fs::metadata(&keyed.compile.output).ok().map(|meta| meta.len());
                    event.append(&events_path(cc_dir));
                    let shown = |text: &[u8]| match keyed.map() {
                        Some(_) => key::messages_for_this_build(conf, text),
                        None => text.to_vec(),
                    };
                    let _ = std::io::stdout().lock().write_all(&shown(&messages.stdout));
                    let _ = std::io::stderr().lock().write_all(&shown(&messages.stderr));
                    keyed.clean_up();
                    std::process::exit(0)
                }
                Err(e) => event.store = Some(format!("the dependency file could not be put in place: {e}")),
            },
            Err(Miss::Absent) => {}
            Err(miss) => event.store = Some(miss.to_string()),
        }
    }
    // Not served: the compile writes its own dependency file. (Audit mode
    // keeps the key's, to compare.)
    if let Ok(keyed) = &mut keyed
        && audited.is_none()
    {
        keyed.drop_depend();
    }
    // The check after the compile looks the inputs up again where it can,
    // which needs them looked up right before the compile (§18.5).
    let ((), looking_ms) = timed(|| {
        if let Ok(keyed) = &mut keyed {
            keyed.before_compile();
        }
    });

    // The compile, as the key describes it when its result is to be stored
    // (the path map's flags; for Fortran, the renamed copy, §18.10): the
    // stored object must be the one the key describes.
    let publishing = store.is_ok() && keyed.is_ok();
    let (compile_argv, rewrite) = match (&keyed, publishing) {
        (Ok(keyed), true) => (keyed.compile_argv(argv), keyed.rewrite(conf)),
        _ => (argv.to_vec(), None),
    };
    // The file that was identified is the file that runs.
    let identified = keyed.as_ref().ok().map(|keyed| keyed.compiler.path.clone());
    let streams = if publishing { Output::PassOn(rewrite.clone()) } else { Output::Inherit };
    let (ran, compile_ms) = timed(|| run(job, &compile_argv, identified.as_deref(), streams));
    let Some((status, captured)) = ran else {
        drop(audited);
        if let Ok(keyed) = &mut keyed {
            keyed.drop_depend();
            keyed.clean_up();
        }
        leave_to(job, conf, cc_dir, "the compiler cannot be started directly, so the recipe's shell runs it")
    };
    event.compile_ms = compile_ms;
    event.exit = status.code();
    event.signal = status.signal();
    // A stop signal from here on ends this process on the spot (see `run`):
    // nothing below is worth making `make` wait for.
    let (stable, recheck_ms) = timed(|| match &mut keyed {
        Ok(keyed) if status.success() => Some(keyed.still_holds()),
        _ => None,
    });
    event.stable = stable;
    event.recheck_ms = looking_ms + recheck_ms;
    if let (Ok(keyed), Some(_)) = (&keyed, stable) {
        match keyed.check_made() {
            Ok(count) => event.lookups = Some(count),
            Err(why) => event.checked_by_compiler = Some(why),
        }
    }
    event.object_bytes = output.as_ref().and_then(|output| output.metadata().ok()).map(|meta| meta.len());

    if let (Some((stored, modules)), Ok(keyed)) = (&audited, &mut keyed) {
        let checked = Checked { status, stable, stored, modules, cwd: &cwd };
        event.audit = Some(audit(job, &compile_argv, identified.as_deref(), rewrite.clone(), &checked, keyed));
        keyed.drop_depend();
    }
    drop(audited);

    // An entry audit mode has shown to be wrong goes, and the fresh object
    // takes its place.
    let wrong = event.audit == Some(Audit::WrongHit);
    if wrong
        && let (Ok(store), Some(key)) = (&store, &event.key)
        && let Err(e) = store.remove(key)
    {
        event.store = Some(format!("the wrong entry could not be removed: {e:#}"));
    }
    if let (Ok(store), Ok(keyed), Some(captured)) = (&store, &keyed, &captured)
        && status.success()
        && stable == Some(true)
        && (event.outcome != Some(Outcome::Hit) || wrong)
    {
        let (published, publish_ms) = timed(|| publish(store, keyed, captured, &event, &cwd));
        event.publish_ms = publish_ms;
        match published {
            Ok(Published::Stored) => event.published = Some(true),
            // Another build compiled the same thing and published it first.
            Ok(Published::AlreadyThere) => event.store = Some("another build published it first".to_owned()),
            Err(why) => {
                event.published = Some(false);
                event.store = Some(why);
            }
        }
    }
    event.append(&events_path(cc_dir));
    if let Ok(keyed) = &mut keyed {
        keyed.clean_up();
    }
    #[cfg(debug_assertions)]
    test_panic("after");
    leave_as(status)
}

/// A temporary file beside `object`, for the stored object in audit mode.
fn beside(object: &Path) -> std::io::Result<tempfile::TempPath> {
    let dir = object.parent().filter(|dir| !dir.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = object.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    Ok(tempfile::Builder::new().prefix(&format!(".{name}.cactup-")).tempfile_in(dir)?.into_temp_path())
}

/// The first compile of an audited hit, as it ended: its status, whether
/// its inputs held still, and where the stored object was put.
struct Checked<'a> {
    status: ExitStatus,
    stable: Option<bool>,
    stored: &'a Path,
    /// The stored module files, put aside, and the directory the compile
    /// writes its own into (§18.10).
    modules: &'a [(String, tempfile::TempPath)],
    cwd: &'a Path,
}

/// Check a hit in audit mode (§18.8): the compile has run as `checked`
/// says. Only a compile whose inputs held still can say anything about the
/// entry. The same bytes, and the dependency file the hit would have
/// written the same as the compile's: `Same`. A different object: the
/// fresh one is moved aside and the compile runs once more; the same object
/// twice (its inputs still unchanged) means the entry was wrong, two
/// objects that the compiler is not deterministic. The build keeps the
/// fresh object either way. A stop signal that reaches this process during
/// the second compile ends it by that signal, as it would have the compile;
/// a second compile that ends any other way (killed by the kernel, crashed,
/// failed) leaves the first compile's object the build's, and no verdict.
fn audit(job: &Job, argv: &[OsString], identified: Option<&Path>, rewrite: Option<key::Rewrite>, checked: &Checked, keyed: &mut key::Keyed) -> Audit {
    if !checked.status.success() {
        return Audit::CompileFailed;
    }
    if checked.stable != Some(true) {
        return Audit::InputsChanged;
    }
    let object = &keyed.compile.output.clone();
    let read = |path: &Path| std::fs::read(path).ok();
    // The module files the compile wrote, as they are now.
    let modules = keyed.modules().to_vec();
    let modules_now = || -> Vec<Option<Vec<u8>>> { modules.iter().map(|name| read(&checked.cwd.join(name))).collect() };
    let fresh = read(object);
    let fresh_modules = modules_now();
    let stored_modules: Vec<Option<Vec<u8>>> = keyed
        .modules()
        .iter()
        .map(|name| checked.modules.iter().find(|(stored, _)| stored == name).and_then(|(_, path)| read(path)))
        .collect();
    if fresh.is_some() && fresh == read(checked.stored) && fresh_modules.iter().all(Option::is_some) && fresh_modules == stored_modules {
        return match keyed.depend_files() {
            Some((ours, compiles)) if read(ours).is_none() || read(ours) != read(compiles) => Audit::WrongDependencyFile,
            _ => Audit::Same,
        };
    }
    let Ok(first) = beside(object) else { return Audit::NotDeterministic };
    if std::fs::rename(object, &first).is_err() {
        return Audit::NotDeterministic;
    }
    // A test can have the second compile be a program that kills itself.
    #[cfg(debug_assertions)]
    let again_as = std::env::var_os("CACTUP_CC_TEST_SECOND_COMPILER").map(PathBuf::from);
    #[cfg(debug_assertions)]
    let identified = again_as.as_deref().or(identified);
    let again = run(job, argv, identified, Output::Swallow(rewrite));
    // Asked to stop, however the second compile then ended (a driver may
    // exit with a status rather than by the signal): the compile would have
    // been stopped.
    if let signal @ 1.. = PENDING.load(Ordering::SeqCst) {
        drop(first);
        match again {
            Some((again, _)) if again.signal().is_some() => leave_as(again),
            _ => {
                let _ = signal_hook::low_level::emulate_default_handler(signal);
                std::process::exit(128 + signal)
            }
        }
    }
    match again {
        Some((again, _)) if again.success() && !keyed.still_holds() => Audit::InputsChanged,
        Some((again, _)) if again.success() && read(object).is_some() && read(object) == read(&first) && modules_now() == fresh_modules => {
            Audit::WrongHit
        }
        Some((again, _)) if again.success() => Audit::NotDeterministic,
        // The second compile failed, was killed, or could not run: the
        // first object is the build's.
        _ => {
            let _ = std::fs::rename(&first, object);
            Audit::SecondCompileFailed
        }
    }
}

/// Publish the object of a compile that succeeded and whose key held, with
/// the messages it wrote (§18.7, §18.8).
fn publish(store: &Store, keyed: &key::Keyed, captured: &Captured, event: &Event, cwd: &Path) -> Result<Published, String> {
    if captured.kept_open {
        return Err("a process the compiler started kept its output open, so not all of it could be stored".to_owned());
    }
    if captured.overflow {
        return Err("the compiler said too much to store".to_owned());
    }
    let stored = |text: &[u8]| match keyed.map() {
        Some(map) => map.messages_for_the_store(text),
        None => text.to_vec(),
    };
    let (stdout, stderr) = (stored(&captured.stdout), stored(&captured.stderr));
    let modules = super::fortran::module_files(cwd, keyed.modules());
    let entry = NewEntry {
        key: event.key.as_deref().unwrap_or_default(),
        parts: &keyed.parts,
        object: &keyed.compile.output,
        stdout: &stdout,
        stderr: &stderr,
        modules: &modules,
        about: About {
            unit: event.unit.clone(),
            compiler: event.compiler.clone(),
            relocatable: keyed.relocatable(),
            cactup: crate::build_info::LONG_VERSION.lines().next().unwrap_or_default().to_owned(),
            host: gethostname::gethostname().to_string_lossy().into_owned(),
        },
    };
    store.publish(&entry).map_err(|e| format!("not published: {e:#}"))
}

/// The compiler, started directly: the file `identified` if the key was
/// computed for one (under the name the recipe gave, as a `PATH` search
/// would have started it), else whatever the name leads to now.
///
/// A shell that sets `_` for the commands it starts (bash does) set it to
/// this wrapper; the compiler is given what that shell would have given
/// it, its own path. Under a shell that leaves `_` alone, so does this.
fn direct(argv: &[OsString], identified: Option<&Path>) -> Command {
    let mut command = match identified {
        Some(file) => {
            let mut command = Command::new(file);
            command.arg0(&argv[0]);
            command
        }
        None => Command::new(&argv[0]),
    };
    command.args(&argv[1..]);
    let set_for_us = std::env::var_os("_").zip(std::env::args_os().next()).is_some_and(|(set, me)| {
        set == me || std::env::current_exe().is_ok_and(|exe| Path::new(&set) == exe)
    });
    if set_for_us {
        match identified.map(Path::to_owned).ok_or(()).or_else(|()| super::identity::find_program(&argv[0])) {
            Ok(program) => command.env("_", program),
            Err(_) => command.env_remove("_"),
        };
    }
    command
}

/// Pass a compile through that the cache stays out of for a reason worth
/// knowing afterward, and say so in the log first: a build whose every
/// compile went this way would otherwise have recorded nothing, and nobody
/// could tell why. What happens to the compile is not known here (this
/// process becomes it), so the line has a reason and nothing else.
fn leave_to(job: &Job, conf: &BuildConf, cc_dir: &Path, why: &str) -> ! {
    log_left(job, conf, cc_dir, why);
    pass_through(job)
}

/// [`leave_to`], for a compile whose compiler only the recipe's shell can
/// say: it goes to a shell even though the words look plain.
fn leave_to_shell(job: &Job, conf: &BuildConf, cc_dir: &Path, why: &str) -> ! {
    log_left(job, conf, cc_dir, why);
    hand_to_shell(job)
}

/// The log's line for a compile the cache stays out of.
fn log_left(job: &Job, conf: &BuildConf, cc_dir: &Path, why: &str) {
    debug(why);
    if conf.mode != Mode::Off {
        let event = Event {
            compiler: job.compiler.to_string_lossy().into_owned(),
            unit: output_of(&job.args).and_then(|output| unit(conf, &output)),
            not_cached: Some(why.to_owned()),
            ..Default::default()
        };
        event.append(&events_path(cc_dir));
    }
}

/// What `work` returns, and the wall-clock milliseconds it took.
fn timed<T>(work: impl FnOnce() -> T) -> (T, u64) {
    let started = Instant::now();
    let done = work();
    (done, started.elapsed().as_millis() as u64)
}

/// The value of `-o` on a compiler command line, read without knowing any
/// other flag (for a compile the cache has no reader for).
fn output_of(args: &[OsString]) -> Option<PathBuf> {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_bytes().strip_prefix(b"-o") {
            Some(b"") => return args.next().map(PathBuf::from),
            Some(joined) => return Some(PathBuf::from(OsStr::from_bytes(joined))),
            None => {}
        }
    }
    None
}

/// The name of the compile that writes `output`: the object's path below
/// the configuration's `build` directory, which is the same in every
/// build of every configuration of every installation.
fn unit(conf: &BuildConf, output: &Path) -> Option<String> {
    let output = std::path::absolute(output).ok()?;
    let build = conf.config_dir.join("build");
    let physical = std::fs::canonicalize(&build).ok();
    let below = [Some(build), physical].into_iter().flatten().find_map(|dir| output.strip_prefix(dir).ok().map(Path::to_owned));
    Some(below?.to_string_lossy().into_owned())
}

/// Become the compiler: the same process, so the same stdin, signal
/// dispositions and make jobserver file descriptors, with nothing of cactup
/// left in between.
fn pass_through(job: &Job) -> ! {
    if let Some(argv) = job.argv() {
        // Returns only if the program could not be started this way; the
        // recipe's shell may still know how.
        let _ = direct(&argv, None).exec();
    }
    hand_to_shell(job)
}

/// What `make` would have handed the shell: the compiler variable's value
/// as shell text (so its quotes, its keywords and its `PATH` lookup mean
/// what they meant in the recipe), then the arguments, which `"$@"` passes
/// on untouched. It is a new shell of the recipe's kind, not the recipe's
/// own: the recipe's shell variables are not there for the text to use.
fn shell_command(job: &Job) -> Command {
    let mut script = job.compiler.clone();
    script.push(" \"$@\"");
    let mut command = Command::new(&job.shell);
    command.arg("-c").arg(script).arg(&job.shell).args(&job.args);
    command
}

/// Let a shell of the recipe's kind run the compile, as this process. If
/// the compiler cannot be run at all, the message and the exit status are
/// the shell's, as they are without cactup.
fn hand_to_shell(job: &Job) -> ! {
    let err = shell_command(job).exec();
    eprintln!("cactup: {}: {err}", job.shell.display());
    std::process::exit(if err.kind() == std::io::ErrorKind::NotFound { 127 } else { 126 });
}

/// The signals this process was started ignoring: the `SigIgn` mask in
/// `/proc/self/status`.
fn ignored_signals() -> Res<u64> {
    let status = std::fs::read_to_string("/proc/self/status").context("Failed to read /proc/self/status")?;
    let mask = status.lines().find_map(|line| line.strip_prefix("SigIgn:"));
    let mask = mask.context("/proc/self/status has no SigIgn line")?;
    u64::from_str_radix(mask.trim(), 16).context("/proc/self/status has a SigIgn line that is not a hex mask")
}

/// Pass `signal` on to the compiler, or to the preprocessor run that
/// checks the key after it. Runs in signal context: atomics and `kill(2)`,
/// nothing else.
fn pass_on(signal: i32) {
    let Some(signal) = rustix::process::Signal::from_named_raw(signal) else { return };
    for child in [&CHILD, &key::PREPROCESSOR] {
        if let Some(child) = rustix::process::Pid::from_raw(child.load(Ordering::SeqCst)) {
            let _ = rustix::process::kill_process(child, signal);
        }
    }
}

/// What a compile wrote to its stdout and stderr, kept while it was passed
/// on (§18.8).
#[derive(Debug, Default)]
struct Captured {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// More than [`MESSAGES_CAP`] of one of them: not all of it was kept.
    overflow: bool,
    /// A process the compiler started still held one of them open when the
    /// compiler had ended: what it says later is not in here.
    kept_open: bool,
}

/// What becomes of a compile's stdout and stderr. Through this process,
/// names in them may be rewritten, line by line: those of a compile of a
/// renamed copy (§18.10).
#[derive(Debug, Clone)]
enum Output {
    /// The recipe's own, inherited: nothing passes through this process.
    Inherit,
    /// Through this process, passed on as it comes and kept (§18.8).
    PassOn(Option<key::Rewrite>),
    /// Through this process and kept, not passed on: a compile audit mode
    /// runs a second time, whose messages were shown the first time.
    Swallow(Option<key::Rewrite>),
}

/// A piece of a stream for [`write_out`], or the word to stop.
enum Piece {
    Bytes(Vec<u8>),
    Stop,
}

/// Read `from` to its end, handing each piece to `to` as it comes and
/// keeping a copy of up to [`MESSAGES_CAP`] bytes. Reading never waits for
/// writing: a slow terminal holds up [`write_out`], not this. With
/// `rewrite`, pieces are whole lines, rewritten (a name never spans lines),
/// and what is kept is what was passed on.
fn read_and_keep(mut from: impl Read + Send + 'static, to: std::sync::mpsc::Sender<Piece>, rewrite: Option<key::Rewrite>) -> std::io::Result<std::thread::JoinHandle<(Vec<u8>, bool)>> {
    std::thread::Builder::new().spawn(move || {
        let (mut kept, mut overflow) = (Vec::new(), false);
        let mut keep = |piece: Vec<u8>| {
            match kept.len() + piece.len() <= MESSAGES_CAP {
                true => kept.extend_from_slice(&piece),
                false => overflow = true,
            }
            let _ = to.send(Piece::Bytes(piece));
        };
        let mut buf = vec![0u8; 64 * 1024];
        let mut pending = Vec::new();
        loop {
            let n = match from.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            match &rewrite {
                None => keep(buf[..n].to_vec()),
                Some(rewrite) => {
                    pending.extend_from_slice(&buf[..n]);
                    if let Some(end) = pending.iter().rposition(|b| *b == b'\n') {
                        let rest = pending.split_off(end + 1);
                        keep(rewrite.apply(&pending));
                        pending = rest;
                    }
                }
            }
        }
        if let Some(rewrite) = &rewrite
            && !pending.is_empty()
        {
            keep(rewrite.apply(&pending));
        }
        (kept, overflow)
    })
}

/// Write every piece that comes to `to`, until told to stop or until no
/// more can come.
fn write_out(mut to: impl Write + Send + 'static, pieces: std::sync::mpsc::Receiver<Piece>) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().spawn(move || {
        while let Ok(Piece::Bytes(bytes)) = pieces.recv() {
            let _ = to.write_all(&bytes).and_then(|()| to.flush());
        }
    })
}

/// One of the compiler's streams on its way through this process.
struct Passing {
    reader: std::thread::JoinHandle<(Vec<u8>, bool)>,
    writer: Option<std::thread::JoinHandle<()>>,
    stop: std::sync::mpsc::Sender<Piece>,
}

impl Passing {
    /// `Err` if a thread cannot be made; whatever was made then ends by
    /// itself once `from`'s other end is gone.
    fn new(from: impl Read + Send + 'static, to: impl Write + Send + 'static, pass_on: bool, rewrite: Option<key::Rewrite>) -> std::io::Result<Self> {
        let (stop, pieces) = std::sync::mpsc::channel();
        let writer = match pass_on {
            true => Some(write_out(to, pieces)?),
            false => None,
        };
        let reader = read_and_keep(from, stop.clone(), rewrite)?;
        Ok(Self { reader, writer, stop })
    }

    /// Once the compiler has ended: what was read by `deadline` (the stream
    /// closes when the compiler ends, unless something it started keeps it
    /// open, which is not waited out), all of it written out first, however
    /// long that takes, as the compiler's own writes would have. `None`:
    /// the stream was still open at the deadline.
    fn finish(self, deadline: Instant) -> Option<(Vec<u8>, bool)> {
        while !self.reader.is_finished() && Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let read = match self.reader.is_finished() {
            true => self.reader.join().ok(),
            false => None,
        };
        let _ = self.stop.send(Piece::Stop);
        if let Some(writer) = self.writer {
            let _ = writer.join();
        }
        read
    }
}

/// Have the stop signals passed on to the compiler (see [`run`]): once per
/// process, however many compiles it runs (audit mode may run two).
static HANDLERS: OnceLock<bool> = OnceLock::new();

/// Run the compiler as a child and wait for it, passing on every signal
/// that asks this process to stop. `output` says whether the compiler's
/// stdout and stderr come through this process, to be kept (§18.8).
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
///
/// A terminal sends its signals to the whole foreground process group, so
/// the compiler gets a Ctrl-C twice: once from the terminal, once passed
/// on. It dies of the first.
///
/// `None`: the compiler could not be started this way, and nothing has
/// run. The recipe's shell may still know how (a keyword such as `time`, a
/// function, a script without an interpreter line).
fn run(job: &Job, argv: &[OsString], identified: Option<&Path>, output: Output) -> Option<(ExitStatus, Option<Captured>)> {
    // The build script's self-test has checked that this can be read here.
    let Ok(ignored) = ignored_signals() else {
        debug("cannot tell which signals to leave ignored");
        pass_through(job)
    };
    let watched = *HANDLERS.get_or_init(|| {
        [SIGHUP, SIGINT, SIGQUIT, SIGTERM].into_iter().filter(|signal| ignored & (1 << (signal - 1)) == 0).all(|signal| {
            let on_signal = move || {
                if PHASE.load(Ordering::SeqCst) == AFTER_COMPILE {
                    // The compiler has finished; what still runs is this
                    // process's own check of the key, or the publishing.
                    // End it and go, the way a compiler still running would
                    // have gone: by the signal, so that `make` discards the
                    // object as unfinished. (The preprocessor's driver is
                    // ended; a back end it started finds its output closed.)
                    pass_on(signal);
                    if signal != SIGQUIT {
                        let _ = signal_hook::low_level::emulate_default_handler(signal);
                    }
                    signal_hook::low_level::exit(128 + signal);
                }
                PENDING.store(signal, Ordering::SeqCst);
                pass_on(signal);
            };
            // SAFETY: the handler touches atomics, makes raw `kill` system
            // calls (rustix makes them without libc), and ends the process
            // by the signal's default action or `_exit`: all
            // async-signal-safe.
            unsafe { signal_hook::low_level::register(signal, on_signal) }.is_ok()
        })
    });
    if !watched {
        debug("cannot watch for signals");
        pass_through(job)
    }

    // The handlers above do not survive becoming the shell that runs what
    // cannot be started here.
    let mut command = direct(argv, identified);
    // Through this process: the pipes, and the threads that read them, are
    // made before the compiler starts, so that what cannot be made costs
    // the keeping (the streams are then the recipe's), never the compile.
    let mut passing = None;
    let through = match output {
        Output::Inherit => None,
        Output::PassOn(rewrite) => Some((true, rewrite)),
        Output::Swallow(rewrite) => Some((false, rewrite)),
    };
    let renamed = through.as_ref().is_some_and(|(_, rewrite)| rewrite.is_some());
    let shown = through.as_ref().is_some_and(|(pass_on, _)| *pass_on);
    if let Some((pass_on, rewrite)) = through
        && let (Ok((out_from, out_to)), Ok((err_from, err_to))) = (std::io::pipe(), std::io::pipe())
    {
        if let (Ok(stdout), Ok(stderr)) = (
            Passing::new(out_from, std::io::stdout(), pass_on, rewrite.clone()),
            Passing::new(err_from, std::io::stderr(), pass_on, rewrite),
        ) {
            command.stdout(Stdio::from(out_to)).stderr(Stdio::from(err_to));
            passing = Some((stdout, stderr));
        }
    }
    // A compile that names its files otherwise than the recipe (§18.10)
    // must have its messages rewritten on their way: without the pipes, it
    // is the recipe's own compile that runs.
    if renamed && passing.is_none() {
        // Audit mode's second compile is not shown at all.
        if !shown {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        } else {
            debug("its messages cannot be passed on, and they would name its files otherwise");
            pass_through(job)
        }
    }
    PHASE.store(BEFORE_COMPILE, Ordering::SeqCst);
    let spawned = command.spawn();
    // This process's copies of the pipes' writing ends: the compiler has
    // its own, and the readers see the end of each stream when it ends.
    drop(command);
    let Ok(mut child) = spawned else {
        // Asked to stop before there was a compile to stop: becoming the
        // shell now would lose that signal and run the compile after all.
        if let signal @ 1.. = PENDING.load(Ordering::SeqCst) {
            let _ = signal_hook::low_level::emulate_default_handler(signal);
            std::process::exit(128 + signal);
        }
        return None;
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
            // The pid is free to be someone else's from here on.
            CHILD.store(0, Ordering::SeqCst);
            PHASE.store(AFTER_COMPILE, Ordering::SeqCst);
            let captured = passing.map(|(stdout, stderr)| {
                let deadline = Instant::now() + std::time::Duration::from_secs(2);
                let (stdout, stderr) = (stdout.finish(deadline), stderr.finish(deadline));
                let kept_open = stdout.is_none() || stderr.is_none();
                let ((stdout, over_out), (stderr, over_err)) = (stdout.unwrap_or_default(), stderr.unwrap_or_default());
                Captured { stdout, stderr, overflow: over_out || over_err, kept_open }
            });
            Some((status, captured))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn job(compiler: &str, args: &[&str]) -> Job {
        Job {
            compiler: OsString::from(compiler),
            args: args.iter().map(OsString::from).collect(),
            shell: PathBuf::from("/bin/bash"),
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

    #[test]
    fn a_name_the_shell_has_its_own_meaning_for_is_not_plain() {
        for compiler in ["time gcc", "command gcc", "exec gcc", "builtin echo", "test", "[ -f x ]", "true"] {
            assert_eq!(argv(compiler, &["-c"]), None, "{compiler}");
        }
        // Only as the command's name, and only as a bare name.
        assert!(argv("gcc time", &[]).is_some());
        assert!(argv("/usr/bin/time gcc", &[]).is_some());
        assert!(argv("./time", &[]).is_some());
        assert!(shell_resolves_differently(OsStr::new("time")));
        assert!(!shell_resolves_differently(OsStr::new("/usr/bin/time")));
    }

    #[test]
    fn finds_the_output_and_names_the_unit() {
        let os = |args: &[&str]| args.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(output_of(&os(&["-c", "-o", "/b/T/a.c.o", "a.c"])), Some(PathBuf::from("/b/T/a.c.o")));
        assert_eq!(output_of(&os(&["-c", "-o/b/T/a.c.o", "a.c"])), Some(PathBuf::from("/b/T/a.c.o")));
        assert_eq!(output_of(&os(&["-c", "a.c"])), None);
        assert_eq!(output_of(&os(&["-c", "a.c", "-o"])), None);

        let conf = BuildConf {
            mode: Mode::Record,
            cactup: PathBuf::from("/opt/cactup"),
            config_dir: PathBuf::from("/w/Cactus/configs/sim"),
            cactus_root: PathBuf::from("/w/Cactus"),
            machine: "m".into(),
            universe: None,
            build_env_digest: String::new(),
            store: PathBuf::from("/nonexistent/cache"),
            relocate: true,
            size_limit: None,
        };
        let name = |output: &str| unit(&conf, Path::new(output));
        assert_eq!(name("/w/Cactus/configs/sim/build/Boundary/a.c.o").as_deref(), Some("Boundary/a.c.o"));
        assert_eq!(name("/w/Cactus/configs/sim/build/T/sub/a.F90.o").as_deref(), Some("T/sub/a.F90.o"));
        assert_eq!(name("/w/Cactus/configs/other/build/T/a.c.o"), None);
        assert_eq!(name("/tmp/conftest.o"), None);
    }

    #[test]
    fn the_shell_gets_the_compiler_as_text_and_the_arguments_as_arguments() {
        let command = shell_command(&job("time gcc -DX=\"a b\"", &["-c", "my file.c"]));
        assert_eq!(command.get_program(), "/bin/bash");
        let args: Vec<&OsStr> = command.get_args().collect();
        // `-c <script> <name for $0> <arguments for "$@">`
        assert_eq!(args, ["-c", "time gcc -DX=\"a b\" \"$@\"", "/bin/bash", "-c", "my file.c"]);
    }
}
