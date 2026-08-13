use colored::Colorize;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

/// Whether the global `--trace` flag is set. A process-global switch (rather
/// than threading the flag through every `Ctx`/scheduler/build call) because
/// command tracing is a cross-cutting concern touched at a dozen spawn sites.
static TRACE: AtomicBool = AtomicBool::new(false);

/// Set by `main()` once from the parsed `--trace` flag.
pub fn set_trace(on: bool) {
    TRACE.store(on, Ordering::Relaxed);
}

/// When `--trace` is on, print `cmd` to stderr exactly as it is about to be
/// spawned — program, args, and (if set) working directory — then let the
/// caller run it. A no-op otherwise. Call immediately before
/// `.output()`/`.status()`/`.spawn()` so the trace reflects reality.
pub fn trace_command(cmd: &Command) {
    if !TRACE.load(Ordering::Relaxed) {
        return;
    }
    let mut parts = vec![sh_quote(&cmd.get_program().to_string_lossy())];
    parts.extend(cmd.get_args().map(|a| sh_quote(&a.to_string_lossy())));
    let mut line = parts.join(" ");
    if let Some(dir) = cmd.get_current_dir() {
        line = format!("cd {} && {line}", sh_quote(&dir.to_string_lossy()));
    }
    // Stderr so it interleaves with the command's own output but never
    // pollutes anything parsing cactup's stdout.
    eprintln!("{} {}", "+".yellow().bold(), line.dimmed());
}

/// Render one argument for the trace line: bare if it is a "safe" shell token,
/// otherwise single-quoted (with embedded quotes escaped). Multi-line snippets
/// — e.g. an env-setup'd `/bin/sh -c` script — stay literal inside the quotes,
/// so the traced line remains copy-pasteable into a shell.
fn sh_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'/' | b'.' | b'=' | b':' | b'@' | b'%' | b'+' | b',')
        });
    if safe {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Expand `$VAR`/`${VAR}` references against the current environment.
/// Undefined variables expand to the empty string, matching shell behaviour.
pub fn expand_env_vars(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }

        match chars.peek() {
            // ${NAME}
            Some('{') => {
                chars.next(); // consume '{'
                let mut name = String::new();
                let mut closed = false;
                while let Some(&nc) = chars.peek() {
                    chars.next();
                    if nc == '}' {
                        closed = true;
                        break;
                    }
                    name.push(nc);
                }
                if closed {
                    out.push_str(&std::env::var(&name).unwrap_or_default());
                } else {
                    // Unterminated `${...` — leave it untouched.
                    out.push_str("${");
                    out.push_str(&name);
                }
            }
            // $NAME (alphanumeric/underscore, not starting with a digit)
            Some(&c2) if c2 == '_' || c2.is_ascii_alphabetic() => {
                let mut name = String::new();
                while let Some(&nc) = chars.peek() {
                    if nc == '_' || nc.is_ascii_alphanumeric() {
                        name.push(nc);
                        chars.next();
                    } else {
                        break;
                    }
                }
                out.push_str(&std::env::var(&name).unwrap_or_default());
            }
            // A lone `$` (or `$` followed by punctuation) is emitted literally.
            _ => out.push('$'),
        }
    }

    out
}

/// Expand a user-supplied path string the way a shell would: a leading `~`
/// becomes the home directory and `$VAR`/`${VAR}` are substituted. Paths typed
/// at our prompts are read straight from stdin with no shell involved, so
/// without this a literal `~` directory would be created in the current
/// working directory. Values passed as flags are already shell-expanded, so
/// running them through this again is a harmless no-op.
pub fn expand_path(input: &str) -> String {
    let expanded = expand_env_vars(input);
    let Some(home) = std::env::home_dir() else {
        return expanded;
    };
    let home = home.as_path();

    if expanded == "~" {
        home.to_string_lossy().into_owned()
    } else if let Some(rest) = expanded.strip_prefix("~/") {
        home.join(rest).to_string_lossy().into_owned()
    } else {
        expanded
    }
}