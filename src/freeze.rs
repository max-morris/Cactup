//! The cactup a generated script runs: the value of `@CACTUP@`.
//!
//! A distribution build lives at a versioned path, `$CACTUP_HOME/bin/cactup-<build>`,
//! behind a `bin/cactup` symlink that self-update moves. Scripts must name
//! the versioned file, not the symlink, so a queued or running job keeps the
//! exact build it was submitted with even after an update. `current_exe()`
//! reads `/proc/self/exe`, which already resolves the symlink; a binary that
//! is not yet versioned (a plain-file `bin/cactup` from an older install, or
//! a copy placed elsewhere) freezes a versioned copy of itself next to itself
//! first. A dev build just names itself.

use crate::build_info::Stamp;
use crate::Res;
use anyhow::{bail, Context};
use colored::Colorize;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The path generated scripts should run cactup by (`@CACTUP@`), computed
/// once per process. Never fails: when no versioned copy can be made (a
/// read-only or full filesystem), it warns once and names the running
/// binary itself.
pub fn frozen_cactup() -> String {
    static FROZEN: OnceLock<String> = OnceLock::new();
    FROZEN
        .get_or_init(|| match std::env::current_exe() {
            Ok(exe) => {
                let stamp = crate::build_info::DIST.filter(|_| crate::build_info::is_dist());
                freeze(&exe, stamp, open_running).display().to_string()
            }
            Err(_) => "cactup".to_owned(),
        })
        .clone()
}

/// [`frozen_cactup`] for an explicit executable and build stamp (`None`: a
/// dev build, which runs itself); `open` opens the bytes to copy.
fn freeze(exe: &Path, stamp: Option<Stamp>, open: impl FnOnce(&Path) -> Res<File>) -> PathBuf {
    let Some(stamp) = stamp else { return exe.to_owned() };
    match versioned_copy(exe, stamp.id, open) {
        Ok(path) => path,
        Err(e) => {
            // An interrupt is not a filesystem problem worth a warning; the
            // command stops at its own next check.
            if !gix::interrupt::is_triggered() {
                eprintln!(
                    "{}",
                    format!(
                        "Warning: could not keep a versioned copy of cactup next to {}: {e:#}. \
                         Jobs submitted now run that file, which a later update may replace.",
                        exe.display()
                    )
                    .yellow()
                );
            }
            exe.to_owned()
        }
    }
}

/// The running binary, opened for copying.
///
/// By its inode, not by whatever `exe` names by now: between
/// `current_exe()` and this open, a concurrent update can rename a symlink
/// over `bin/cactup`, and `cactup-<id>` would then receive another build's
/// bytes under this build's name. `/proc/self/exe` always opens the file
/// this process was started from; `exe` is only the fallback for when
/// /proc is unavailable.
fn open_running(exe: &Path) -> Res<File> {
    match File::open("/proc/self/exe") {
        Ok(file) => Ok(file),
        Err(_) => File::open(exe).with_context(|| format!("Failed to open {}", exe.display())),
    }
}

/// `exe` itself when it is already `cactup-<id>`; else `cactup-<id>` in the
/// same directory, copied from `open(exe)` (temp file, mode 0755, rename)
/// unless it is already there.
fn versioned_copy(exe: &Path, id: &str, open: impl FnOnce(&Path) -> Res<File>) -> Res<PathBuf> {
    let name = format!("cactup-{id}");
    if exe.file_name().is_some_and(|n| n == name.as_str()) {
        return Ok(exe.to_owned());
    }
    let dir = exe
        .parent()
        .with_context(|| format!("{} has no parent directory", exe.display()))?;
    let versioned = dir.join(&name);
    if versioned.is_file() {
        return Ok(versioned);
    }

    let mut temp = tempfile::Builder::new()
        .prefix(".cactup-freeze.")
        .tempfile_in(dir)
        .with_context(|| format!("Failed to create a temp file in {}", dir.display()))?;
    let mut source = open(exe)?;
    std::io::copy(&mut source, &mut temp)
        .with_context(|| format!("Failed to copy {}", exe.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("Failed to make {} executable", temp.path().display()))?;
    }
    // The copy is the long part; an interrupt drops the temp file here.
    if gix::interrupt::is_triggered() {
        bail!("interrupted");
    }
    // A racing process may have put the same bytes there meanwhile; the
    // rename replaces them atomically, which is harmless.
    temp.persist(&versioned)
        .with_context(|| format!("Failed to move the copy into place at {}", versioned.display()))?;
    Ok(versioned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    const STAMP: Stamp = Stamp { id: "abc1234", date: "2026-09-24T00:00:00+00:00" };

    /// Copy the fake binary a test wrote, not the running test executable.
    fn by_path(path: &Path) -> Res<File> {
        Ok(File::open(path)?)
    }

    #[test]
    fn the_copy_source_is_the_running_inode_not_the_path() {
        use std::os::unix::fs::MetadataExt;
        // Whatever the path names now, the source is this process's own file.
        let bin = tempfile::tempdir().unwrap();
        let impostor = bin.path().join("cactup");
        fs::write(&impostor, b"another build").unwrap();
        let source = open_running(&impostor).unwrap().metadata().unwrap();
        let running = fs::metadata(std::env::current_exe().unwrap()).unwrap();
        assert_eq!((source.dev(), source.ino()), (running.dev(), running.ino()));
    }

    #[test]
    fn a_dev_build_runs_itself() {
        let exe = Path::new("/opt/whatever/cactup");
        assert_eq!(freeze(exe, None, by_path), exe);
        // Under test this is never a distribution build: the running test
        // binary names itself.
        let current = std::env::current_exe().unwrap();
        assert_eq!(frozen_cactup(), current.display().to_string());
    }

    #[test]
    fn a_versioned_binary_runs_itself() {
        // Nothing is created: the path need not even exist.
        let exe = Path::new("/nonexistent/bin/cactup-abc1234");
        assert_eq!(freeze(exe, Some(STAMP), by_path), exe);
    }

    #[test]
    fn a_plain_binary_gets_a_versioned_copy_that_is_then_reused() {
        let bin = tempfile::tempdir().unwrap();
        let exe = bin.path().join("cactup");
        fs::write(&exe, b"#!/bin/sh\necho binary\n").unwrap();

        let frozen = freeze(&exe, Some(STAMP), by_path);
        assert_eq!(frozen, bin.path().join("cactup-abc1234"));
        assert_eq!(fs::read(&frozen).unwrap(), fs::read(&exe).unwrap());
        assert_eq!(fs::metadata(&frozen).unwrap().permissions().mode() & 0o777, 0o755);

        // Present already: reused as is, not copied over.
        fs::write(&frozen, b"the earlier copy").unwrap();
        assert_eq!(freeze(&exe, Some(STAMP), by_path), frozen);
        assert_eq!(fs::read(&frozen).unwrap(), b"the earlier copy");

        let mut names: Vec<_> =
            fs::read_dir(bin.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        names.sort();
        assert_eq!(names, ["cactup", "cactup-abc1234"], "no temp files left behind");
    }

    #[test]
    fn an_unwritable_directory_falls_back_to_the_binary() {
        let bin = tempfile::tempdir().unwrap();
        let exe = bin.path().join("cactup");
        fs::write(&exe, b"binary").unwrap();
        fs::set_permissions(bin.path(), fs::Permissions::from_mode(0o555)).unwrap();
        // root ignores directory permissions; the fallback is untestable then.
        let writable = fs::write(bin.path().join("probe"), b"").is_ok();
        if !writable {
            assert_eq!(freeze(&exe, Some(STAMP), by_path), exe);
            assert!(!bin.path().join("cactup-abc1234").exists());
        }
        fs::set_permissions(bin.path(), fs::Permissions::from_mode(0o755)).unwrap();
    }
}
