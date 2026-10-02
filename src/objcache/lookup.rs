//! What the recipe's shell runs for a compiler's bare name (§18.4).
//!
//! The wrapper starts a plain compiler itself, as the file a `PATH` search
//! finds. The recipe's shell might have run something else under that
//! name: a function or alias, or a `hash -p` entry, that it set up for
//! itself at startup. Cactus runs its recipes with bash, and bash reads the
//! file `BASH_ENV` names before every recipe line; module systems set it.
//! Nothing outside the shell can see what that file defines, so the shell
//! is asked (`command -v <name>`), and the wrapper starts the compiler
//! itself only when the answer is the file it found.
//!
//! The answer is remembered per build attempt, beside the compilers'
//! identities (`<attempt>/cc/compilers/`), for as long as what it depends on
//! is what it was: the shell's file, the variables that decide what it
//! reads at startup and where it looks, the files those name, and the
//! working directory when `PATH` has a relative entry.

use super::hash::Hasher;
use super::identity::{find_program, Seen};
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The variables that decide which startup files a shell reads (bash's
/// `BASH_ENV`, the POSIX shells' `ENV`, zsh's `ZDOTDIR` and `HOME`) and where
/// it looks for a name.
const LOOKUP_ENV: &[&str] = &["PATH", "BASH_ENV", "ENV", "ZDOTDIR", "HOME"];

/// One file the answer depends on, as it looked: there or not.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct Watched {
    path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    seen: Option<Seen>,
}

impl Watched {
    fn of(path: &Path) -> Self {
        Self { path: path.to_owned(), seen: Seen::of(path).ok() }
    }

    fn unchanged(&self) -> bool {
        Seen::of(&self.path).ok() == self.seen
    }
}

/// What is kept between the compiles of one build attempt.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct Remembered {
    /// The program a `PATH` search found, by its physical path, when the
    /// shell was asked.
    program: PathBuf,
    /// Why the shell runs something else for the name; none if it runs
    /// `program`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    differs: Option<String>,
    /// The variables of [`LOOKUP_ENV`] (and the working directory if it
    /// matters), digested.
    env: String,
    files: Vec<Watched>,
}

/// The variables of [`LOOKUP_ENV`], and the working directory when `PATH`
/// has a relative entry (the shell then finds names from where it runs).
fn lookup_env() -> String {
    let mut hasher = Hasher::new("lookup-env");
    for name in LOOKUP_ENV {
        hasher.feed(name.as_bytes());
        hasher.feed(std::env::var_os(name).unwrap_or_default().as_bytes());
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    if path.as_bytes().split(|b| *b == b':').any(|dir| !dir.starts_with(b"/")) {
        hasher.feed(b"cwd");
        hasher.feed(std::env::current_dir().unwrap_or_default().as_os_str().as_bytes());
    }
    hasher.hex()
}

/// Does `shell`, the recipe's shell, run the program a `PATH` search finds
/// for the bare name `name`? `Err` says what it runs instead, or why that
/// cannot be told; the compile is then the shell's to run.
pub fn shell_runs_program(cc_dir: &Path, shell: &Path, name: &OsStr) -> Result<(), String> {
    let program = find_program(name)
        .and_then(|found| fs::canonicalize(&found).map_err(Into::into))
        .map_err(|e| format!("{e:#}"))?;
    let memo_dir = cc_dir.join("compilers");
    let mut memo_name = Hasher::new("lookup-memo");
    memo_name.feed(shell.as_os_str().as_bytes());
    memo_name.feed(name.as_bytes());
    let memo = memo_dir.join(format!("shell-{}.toml", memo_name.hex()));
    let env = lookup_env();
    if let Some(remembered) = fs::read_to_string(&memo).ok().and_then(|text| toml::from_str::<Remembered>(&text).ok())
        && remembered.program == program
        && remembered.env == env
        && remembered.files.iter().all(Watched::unchanged)
    {
        return remembered.differs.map_or(Ok(()), Err);
    }

    // What the shell's answer depends on, looked at before it is asked: a
    // change in between then shows as a change next time.
    let mut files = vec![Watched::of(&find_program(shell.as_os_str()).unwrap_or_else(|_| shell.to_owned()))];
    for variable in ["BASH_ENV", "ENV"] {
        if let Some(file) = std::env::var_os(variable).filter(|value| !value.is_empty()) {
            files.push(Watched::of(Path::new(&file)));
        }
    }
    let differs = ask(shell, name, &program).err();
    let remembered = Remembered { program, differs, env, files };
    // Best-effort, like the compilers' identities: without it the next
    // compile asks again.
    if fs::create_dir_all(&memo_dir).is_ok()
        && let Ok(text) = toml::to_string(&remembered)
        && let Ok(mut temp) = tempfile::NamedTempFile::new_in(&memo_dir)
        && temp.write_all(text.as_bytes()).is_ok()
    {
        let _ = temp.persist(&memo);
    }
    remembered.differs.map_or(Ok(()), Err)
}

/// Ask `shell` what it runs for `name`, and compare with `program`.
fn ask(shell: &Path, name: &OsStr, program: &Path) -> Result<(), String> {
    let shown = name.to_string_lossy();
    let out = Command::new(shell)
        .args(["-c", "command -v \"$1\"", "cactup"])
        .arg(name)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("the recipe's shell ({}) could not be asked what {shown} is: {e}", shell.display()))?;
    // The answer is the last line: a startup file may have printed first.
    let said = String::from_utf8_lossy(&out.stdout);
    let answer = said.lines().rev().find(|line| !line.is_empty()).unwrap_or_default();
    if !out.status.success() || answer.is_empty() {
        return Err(format!("the recipe's shell finds no {shown}"));
    }
    let differs = || Err(format!("the recipe's shell runs something else for {shown} ({answer})"));
    // A path, absolute or (from a relative `PATH` entry) from here; anything
    // else is a function, an alias or a builtin.
    if !answer.contains('/') {
        return differs();
    }
    match fs::canonicalize(answer) {
        Ok(found) if found == program => Ok(()),
        _ => differs(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_name_is_the_program_on_path() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(shell_runs_program(tmp.path(), Path::new("/bin/sh"), OsStr::new("sh")), Ok(()));
        // Remembered: one file.
        assert_eq!(fs::read_dir(tmp.path().join("compilers")).unwrap().count(), 1);
        assert_eq!(shell_runs_program(tmp.path(), Path::new("/bin/sh"), OsStr::new("sh")), Ok(()));
        // A name nothing has is no compiler to start.
        let err = shell_runs_program(tmp.path(), Path::new("/bin/sh"), OsStr::new("cactup-no-such-cc")).unwrap_err();
        assert!(err.contains("not on PATH"), "{err}");
    }

    #[test]
    fn a_shell_that_says_something_else_is_believed() {
        let tmp = tempfile::tempdir().unwrap();
        let program = fs::canonicalize(find_program(OsStr::new("sh")).unwrap()).unwrap();
        let shell = |body: &str| {
            let path = tmp.path().join("shell");
            fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
            path
        };
        for (body, expected) in [
            ("echo sh", Err("something else for sh (sh)")),
            ("echo \"alias sh='echo hi'\"", Err("something else for sh (alias sh='echo hi')")),
            ("echo /bin/true", Err("something else for sh (/bin/true)")),
            ("exit 1", Err("finds no sh")),
            ("true", Err("finds no sh")),
            (&format!("echo startup chatter; echo {}", program.display()), Ok(())),
        ] {
            let result = ask(&shell(body), OsStr::new("sh"), &program);
            match expected {
                Ok(()) => assert_eq!(result, Ok(()), "{body}"),
                Err(part) => assert!(result.as_ref().is_err_and(|e| e.contains(part)), "{body}: {result:?}"),
            }
        }
    }
}
