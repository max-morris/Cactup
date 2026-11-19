//! Copying an installation's files to another installation home
//! (`installation cp`, spec §3.1).
//!
//! A copy keeps what a build and git care about: file modes, modification
//! times (make compares them; git's index caches them), and symlinks as
//! symlinks. Arrangement links and the Cactus top level are relative links
//! into `repos/`, so they hold in the copy as written; a link that names a
//! place inside the source home by absolute path is re-pointed at the same
//! place in the copy, so that the copy never quietly reads the source's
//! files. Sockets, FIFOs and devices are left out and listed.

use crate::Res;
use anyhow::{bail, Context};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// Copies entries from one installation home to another. Shared across
/// `par::parallel_map` workers: everything it mutates is behind a lock.
pub(crate) struct Copier<'a> {
    from_home: &'a Path,
    to_home: &'a Path,
    stop: &'a AtomicBool,
    /// Called once per entry copied, for a progress count.
    counted: &'a (dyn Fn() + Sync),
    skipped: Mutex<Vec<PathBuf>>,
}

impl<'a> Copier<'a> {
    /// A copier from the installation home `from_home` to `to_home`: an
    /// absolute link into the first is re-pointed into the second, wherever
    /// the copy is written (it may be written in a staging directory first).
    pub fn new(
        from_home: &'a Path,
        to_home: &'a Path,
        stop: &'a AtomicBool,
        counted: &'a (dyn Fn() + Sync),
    ) -> Copier<'a> {
        Copier { from_home, to_home, stop, counted, skipped: Mutex::new(Vec::new()) }
    }

    /// Copy the entry at `from` to `to`, which must not exist: a directory
    /// with everything under it, a file with its mode and times, a symlink
    /// re-pointed by [`Copier::link_target`]. Fails with "interrupted" as soon
    /// as `stop` is set.
    pub fn copy(&self, from: &Path, to: &Path) -> Res<()> {
        if self.stop.load(Ordering::Relaxed) {
            bail!("Interrupted");
        }
        let meta = fs::symlink_metadata(from).with_context(|| format!("Failed to stat '{}'", from.display()))?;
        let kind = meta.file_type();
        if kind.is_symlink() {
            let target = fs::read_link(from).with_context(|| format!("Failed to read the link '{}'", from.display()))?;
            std::os::unix::fs::symlink(self.link_target(from, &target), to)
                .with_context(|| format!("Failed to create the link '{}'", to.display()))?;
            set_times(to, &meta);
        } else if kind.is_dir() {
            self.make_dir(to)?;
            for entry in fs::read_dir(from).with_context(|| format!("Failed to list '{}'", from.display()))? {
                let entry = entry.with_context(|| format!("Failed to list '{}'", from.display()))?;
                self.copy(&entry.path(), &to.join(entry.file_name()))?;
            }
            self.finish_dir(from, to)?;
        } else if kind.is_file() {
            fs::copy(from, to)
                .with_context(|| format!("Failed to copy '{}' to '{}'", from.display(), to.display()))?;
            set_times(to, &meta);
        } else {
            self.skipped.lock().expect("skipped list poisoned").push(from.to_owned());
            return Ok(());
        }
        (self.counted)();
        Ok(())
    }

    /// Create the directory `to`, empty, to be filled by the caller and then
    /// given its source's mode and times by [`Copier::finish_dir`] (a
    /// read-only source directory would refuse its own entries otherwise).
    pub fn make_dir(&self, to: &Path) -> Res<()> {
        fs::create_dir(to).with_context(|| format!("Failed to create '{}'", to.display()))
    }

    /// Give the filled directory `to` the mode and times of `from`.
    pub fn finish_dir(&self, from: &Path, to: &Path) -> Res<()> {
        let meta = fs::metadata(from).with_context(|| format!("Failed to stat '{}'", from.display()))?;
        fs::set_permissions(to, meta.permissions())
            .with_context(|| format!("Failed to set the mode of '{}'", to.display()))?;
        set_times(to, &meta);
        Ok(())
    }

    /// What the copy of the link at `link` (in the source) points at, given
    /// what it pointed at there. A relative target is kept as written while
    /// it leads somewhere inside the source home, since the copy has the same
    /// layout; one that climbs out of it becomes the absolute path it led to.
    /// An absolute target inside the source home is moved to the same place
    /// in the copy; any other absolute target is kept.
    pub fn link_target(&self, link: &Path, target: &Path) -> PathBuf {
        if target.is_absolute() {
            return match target.strip_prefix(self.from_home) {
                Ok(rest) => self.to_home.join(rest),
                Err(_) => target.to_owned(),
            };
        }
        let led_to = lexical(&link.parent().unwrap_or(Path::new("/")).join(target));
        if led_to.starts_with(self.from_home) { target.to_owned() } else { led_to }
    }

    /// The entries left out: neither a file, a directory nor a symlink.
    pub fn skipped(&self) -> Vec<PathBuf> {
        self.skipped.lock().expect("skipped list poisoned").clone()
    }
}

/// `path` with `.` and `..` resolved by name, without asking the filesystem.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Give `to` the access and modification times in `meta`, not following a
/// symlink. Best effort: times only spare git and make some rework, so a
/// filesystem that refuses them does not fail the copy.
fn set_times(to: &Path, meta: &fs::Metadata) {
    use rustix::fs::{AtFlags, Timespec, Timestamps};
    let times = Timestamps {
        last_access: Timespec { tv_sec: meta.atime(), tv_nsec: meta.atime_nsec() as _ },
        last_modification: Timespec { tv_sec: meta.mtime(), tv_nsec: meta.mtime_nsec() as _ },
    };
    let _ = rustix::fs::utimensat(rustix::fs::CWD, to, &times, AtFlags::SYMLINK_NOFOLLOW);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn copier<'a>(from: &'a Path, to: &'a Path, stop: &'a AtomicBool, counted: &'a (dyn Fn() + Sync)) -> Copier<'a> {
        Copier::new(from, to, stop, counted)
    }

    #[test]
    fn a_tree_is_copied_with_modes_times_and_links() {
        let tmp = tempfile::tempdir().unwrap();
        let from = tmp.path().join("a");
        let to = tmp.path().join("b");
        let tree = from.join("Cactus");
        fs::create_dir_all(tree.join("repos/flesh/bin")).unwrap();
        fs::create_dir_all(tree.join("arrangements/Base")).unwrap();
        fs::write(tree.join("repos/flesh/Makefile"), "all:\n").unwrap();
        fs::write(tree.join("repos/flesh/bin/run"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(tree.join("repos/flesh/bin/run"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(tree.join("repos/flesh/pack"), "objects").unwrap();
        fs::set_permissions(tree.join("repos/flesh/pack"), fs::Permissions::from_mode(0o444)).unwrap();
        symlink("repos/flesh/Makefile", tree.join("Makefile")).unwrap();
        symlink("../../repos/flesh", tree.join("arrangements/Base/Flesh")).unwrap();
        symlink(tree.join("repos/flesh/Makefile"), tree.join("abs-inside")).unwrap();
        symlink("/usr/share/outside", tree.join("abs-outside")).unwrap();
        symlink("../../elsewhere/x", tree.join("climbs-out")).unwrap();
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        fs::File::options().write(true).open(tree.join("repos/flesh/Makefile")).unwrap().set_modified(old).unwrap();
        // A read-only directory still gets its entries.
        fs::set_permissions(tree.join("repos/flesh/bin"), fs::Permissions::from_mode(0o555)).unwrap();

        let count = std::sync::atomic::AtomicUsize::new(0);
        let counted = || {
            count.fetch_add(1, Ordering::Relaxed);
        };
        let stop = AtomicBool::new(false);
        fs::create_dir_all(&to).unwrap();
        copier(&from, &to, &stop, &counted).copy(&tree, &to.join("Cactus")).unwrap();
        let copy = to.join("Cactus");

        assert_eq!(fs::read_to_string(copy.join("repos/flesh/Makefile")).unwrap(), "all:\n");
        assert_eq!(fs::metadata(copy.join("repos/flesh/bin/run")).unwrap().permissions().mode() & 0o777, 0o755);
        assert_eq!(fs::metadata(copy.join("repos/flesh/pack")).unwrap().permissions().mode() & 0o777, 0o444);
        assert_eq!(fs::metadata(copy.join("repos/flesh/bin")).unwrap().permissions().mode() & 0o777, 0o555);
        assert_eq!(fs::metadata(copy.join("repos/flesh/Makefile")).unwrap().modified().unwrap(), old);

        assert_eq!(fs::read_link(copy.join("Makefile")).unwrap(), Path::new("repos/flesh/Makefile"));
        assert_eq!(fs::read_link(copy.join("arrangements/Base/Flesh")).unwrap(), Path::new("../../repos/flesh"));
        assert_eq!(fs::read_link(copy.join("abs-inside")).unwrap(), copy.join("repos/flesh/Makefile"));
        assert_eq!(fs::read_link(copy.join("abs-outside")).unwrap(), Path::new("/usr/share/outside"));
        assert_eq!(fs::read_link(copy.join("climbs-out")).unwrap(), tmp.path().join("elsewhere/x"));

        // Cactus, repos, flesh, bin, arrangements, Base: 6 directories; 3
        // files; 5 links.
        assert_eq!(count.load(Ordering::Relaxed), 14);
        fs::set_permissions(tree.join("repos/flesh/bin"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(copy.join("repos/flesh/bin"), fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn a_fifo_is_left_out_and_listed() {
        let tmp = tempfile::tempdir().unwrap();
        let from = tmp.path().join("a");
        fs::create_dir_all(&from).unwrap();
        rustix::fs::mknodat(
            rustix::fs::CWD,
            from.join("pipe"),
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::from_raw_mode(0o600),
            0,
        )
        .unwrap();
        fs::write(from.join("file"), "x").unwrap();
        let to = tmp.path().join("b");
        let stop = AtomicBool::new(false);
        let copier = copier(&from, &to, &stop, &|| ());
        copier.copy(&from, &to).unwrap();
        assert!(to.join("file").is_file());
        assert!(fs::symlink_metadata(to.join("pipe")).is_err());
        assert_eq!(copier.skipped(), [from.join("pipe")]);
    }

    #[test]
    fn a_copy_stops_when_asked() {
        let tmp = tempfile::tempdir().unwrap();
        let from = tmp.path().join("a");
        fs::create_dir_all(from.join("d")).unwrap();
        let to = tmp.path().join("b");
        let stop = AtomicBool::new(true);
        let err = copier(&from, &to, &stop, &|| ()).copy(&from, &to).unwrap_err();
        assert!(format!("{err:#}").contains("Interrupted"), "{err:#}");
        assert!(!to.exists());
    }

    #[test]
    fn lexical_resolves_dots_by_name() {
        assert_eq!(lexical(Path::new("/a/b/../c/./d")), Path::new("/a/c/d"));
        assert_eq!(lexical(Path::new("/a/../../x")), Path::new("/x"));
    }
}
