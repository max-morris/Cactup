//! Copy-on-write for symbolic copies (`installation cp --symbolic`, spec
//! §3.1).
//!
//! A symbolic copy's tree has real directories, and in them an absolute
//! symlink to the same file in the source installation for every file the
//! copy has not changed: a "copy link". Its repos' `.git` directories are
//! real copies (git takes no `HEAD` that is a link to elsewhere, and refs,
//! the index and reflogs are written in place), except the objects, which
//! the source's object store lends through git's own `objects/info/alternates`
//! ([`link_objects`]): git and gix read through it and write new objects to
//! the copy's own store.
//!
//! Writing through a copy link would change the source, so a writer must
//! never open one for writing:
//!
//! - A checkout (gix's, or git's) unlinks a link before it writes the file,
//!   so a file a fetch changes becomes the copy's own, and one it leaves
//!   alone stays a link: copy-on-write by itself.
//! - [`Cow::before_replace`] for a file cactup writes anew whole (a parfile,
//!   a thornlist): the link is removed first.
//! - [`Cow::reify`] for anything changed in place (what a system tool like
//!   svn checks out) and for `installation reify`: every copy link at or
//!   under the path becomes a copy of what it points at, and a repo's
//!   borrowed objects become its own ([`materialize_objects`]).
//!
//! In an installation that is not a symbolic copy all of this does nothing.

use crate::installation::Installation;
use crate::Res;
use anyhow::{bail, Context};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// The copy-on-write view of one installation: where its copy links point,
/// if it is a symbolic copy.
#[derive(Debug, Clone, Default)]
pub(crate) struct Cow {
    linked_to: Option<PathBuf>,
}

/// What a [`Cow::reify`] did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Reified {
    /// Links replaced by a copy of the file they pointed at.
    pub copied: usize,
    /// Links removed because the file they pointed at is gone from the
    /// source: the copy never changed that file, and the source deleted it.
    pub dangling: usize,
}

impl std::ops::AddAssign for Reified {
    fn add_assign(&mut self, other: Reified) {
        self.copied += other.copied;
        self.dangling += other.dangling;
    }
}

impl Cow {
    /// The view of an installation whose copy links point into `linked_to`
    /// (none: not a symbolic copy).
    pub fn linked_to(linked_to: Option<PathBuf>) -> Cow {
        Cow { linked_to }
    }

    /// The view of the installation at `home`, from its record.
    pub fn at(home: &Path) -> Res<Cow> {
        Ok(Cow { linked_to: Installation::new("", home).meta()?.linked_to })
    }

    /// The installation home this one's copy links point into.
    pub fn source(&self) -> Option<&Path> {
        self.linked_to.as_deref()
    }

    /// Whether `path` itself is a copy link.
    pub fn is_link(&self, path: &Path) -> bool {
        let Some(source) = &self.linked_to else { return false };
        fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
            && fs::read_link(path).is_ok_and(|target| target.is_absolute() && target.starts_with(source))
    }

    /// Before the file at `path` is written anew whole: a copy link there is
    /// removed, so the write lands in this installation, not in the source.
    pub fn before_replace(&self, path: &Path) -> Res<()> {
        if self.is_link(path) {
            fs::remove_file(path).with_context(|| format!("Failed to remove the link '{}'", path.display()))?;
        }
        Ok(())
    }

    /// Before `path` (a file, or a directory and everything under it) is
    /// changed in place: every copy link at or under it becomes a copy of the
    /// file it points at, with that file's mode and times, and a link to a
    /// file the source no longer has is removed. Each link is replaced in one
    /// rename, so the path never goes missing. `counted` is called per link
    /// handled; `stop` (Ctrl-C) ends it with "interrupted", every link
    /// handled so far staying handled.
    pub fn reify(&self, path: &Path, stop: &AtomicBool, counted: &(dyn Fn() + Sync)) -> Res<Reified> {
        let mut done = Reified::default();
        if self.linked_to.is_some() {
            self.reify_into(path, stop, counted, &mut done)?;
        }
        Ok(done)
    }

    /// [`Cow::reify`] without a count or a way to stop: for one repo or file,
    /// inside work that has its own progress and polls Ctrl-C between items.
    pub fn reify_quietly(&self, path: &Path) -> Res<Reified> {
        self.reify(path, &AtomicBool::new(false), &|| ())
    }

    fn reify_into(&self, path: &Path, stop: &AtomicBool, counted: &(dyn Fn() + Sync), done: &mut Reified) -> Res<()> {
        if stop.load(Ordering::Relaxed) {
            bail!("Interrupted");
        }
        let Ok(meta) = fs::symlink_metadata(path) else { return Ok(()) };
        if meta.file_type().is_symlink() {
            if self.is_link(path) {
                match materialize(path)? {
                    true => done.copied += 1,
                    false => done.dangling += 1,
                }
                counted();
            }
        } else if meta.is_dir() {
            if is_object_store(path) {
                done.copied += materialize_objects(path, stop, counted)?;
            }
            for entry in fs::read_dir(path).with_context(|| format!("Failed to list '{}'", path.display()))? {
                let entry = entry.with_context(|| format!("Failed to list '{}'", path.display()))?;
                self.reify_into(&entry.path(), stop, counted, done)?;
            }
        }
        Ok(())
    }
}

/// What opens the alternates lines a symbolic copy writes, so that a later
/// reify (or a plain copy) can tell them from the user's own.
const ALTERNATES_MARK: &str = "# cactup: borrowed by a symbolic copy until reified";

/// Whether `dir` is a repository's object store: `objects` in a `.git`.
pub(crate) fn is_object_store(dir: &Path) -> bool {
    dir.file_name().is_some_and(|n| n == "objects")
        && dir.parent().and_then(Path::file_name).is_some_and(|n| n == ".git")
}

/// Make `to` (an empty directory) an object store that borrows every object
/// of the store `from`, through `info/alternates`: what a symbolic copy's
/// repo has in place of copies of the source's packs.
pub(crate) fn link_objects(from: &Path, to: &Path) -> Res<()> {
    for dir in [to.join("info"), to.join("pack")] {
        fs::create_dir_all(&dir).with_context(|| format!("Failed to create '{}'", dir.display()))?;
    }
    let alternates = to.join("info/alternates");
    fs::write(&alternates, format!("{ALTERNATES_MARK}\n{}\n", from.display()))
        .with_context(|| format!("Failed to write '{}'", alternates.display()))
}

/// Copy into the object store `objects` every object it borrows through
/// alternates lines [`link_objects`] wrote (and that those stores borrow in
/// turn), then drop those lines: the repo no longer reads another
/// installation's objects. A line of the user's own is kept. Each file is
/// copied under a temporary name and renamed into place, and the lines go
/// only once every object is in, so an interruption leaves a repo that reads
/// as before. Returns the number of files copied.
pub(crate) fn materialize_objects(objects: &Path, stop: &AtomicBool, counted: &(dyn Fn() + Sync)) -> Res<usize> {
    let alternates = objects.join("info/alternates");
    let Ok(text) = fs::read_to_string(&alternates) else { return Ok(0) };
    let (borrowed, kept) = split_alternates(&text);
    if borrowed.is_empty() {
        return Ok(0);
    }
    let mut kept = kept;
    let mut copied = 0;
    let mut pending: Vec<PathBuf> = borrowed;
    let mut seen = Vec::new();
    while let Some(store) = pending.pop() {
        if seen.contains(&store) {
            continue;
        }
        if seen.len() > 40 {
            bail!("'{}' borrows objects 40 stores deep; something is wrong", alternates.display());
        }
        copied += copy_objects(&store, objects, stop, counted)?;
        if let Ok(text) = fs::read_to_string(store.join("info/alternates")) {
            let (more, theirs) = split_alternates(&text);
            pending.extend(more);
            // What that store borrows of its own accord, this one now
            // borrows directly (git reads alternates paths as written).
            for line in theirs.lines().filter(|l| !l.trim().is_empty() && !l.starts_with('#')) {
                if !kept.lines().any(|k| k == line) {
                    kept.push_str(line);
                    kept.push('\n');
                }
            }
        }
        seen.push(store);
    }
    match kept.trim().is_empty() {
        true => fs::remove_file(&alternates),
        false => fs::write(&alternates, kept),
    }
    .with_context(|| format!("Failed to rewrite '{}'", alternates.display()))?;
    Ok(copied)
}

/// The stores `text` (an `info/alternates` file) borrows by
/// [`link_objects`]'s marked lines, and the rest of the file as it was.
fn split_alternates(text: &str) -> (Vec<PathBuf>, String) {
    let mut borrowed = Vec::new();
    let mut kept = String::new();
    let mut marked = false;
    for line in text.lines() {
        if line == ALTERNATES_MARK {
            marked = true;
        } else if marked && !line.trim().is_empty() {
            borrowed.push(PathBuf::from(line.trim()));
            marked = false;
        } else {
            kept.push_str(line);
            kept.push('\n');
        }
    }
    (borrowed, kept)
}

/// Copy the packs and loose objects of the store `from` into the store `to`,
/// leaving any `to` has already; the store's `info/` is not copied.
fn copy_objects(from: &Path, to: &Path, stop: &AtomicBool, counted: &(dyn Fn() + Sync)) -> Res<usize> {
    let mut copied = 0;
    let Ok(entries) = fs::read_dir(from) else { return Ok(0) };
    for entry in entries {
        let entry = entry.with_context(|| format!("Failed to list '{}'", from.display()))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let is_fanout = name.len() == 2 && name.bytes().all(|b| b.is_ascii_hexdigit());
        if name != "pack" && !is_fanout {
            continue;
        }
        let into = to.join(&*name);
        fs::create_dir_all(&into).with_context(|| format!("Failed to create '{}'", into.display()))?;
        for file in fs::read_dir(entry.path()).with_context(|| format!("Failed to list '{}'", entry.path().display()))? {
            if stop.load(Ordering::Relaxed) {
                bail!("Interrupted");
            }
            let file = file.with_context(|| format!("Failed to list '{}'", entry.path().display()))?;
            let target = into.join(file.file_name());
            let meta = fs::metadata(file.path()).with_context(|| format!("Failed to stat '{}'", file.path().display()))?;
            if !meta.is_file() || fs::symlink_metadata(&target).is_ok() {
                continue;
            }
            let temp = tempfile::Builder::new()
                .prefix(".cactup-reify")
                .tempfile_in(&into)
                .with_context(|| format!("Failed to create a file in '{}'", into.display()))?;
            fs::copy(file.path(), temp.path()).with_context(|| format!("Failed to copy '{}'", file.path().display()))?;
            crate::treecopy::set_times(temp.path(), &meta);
            temp.persist(&target).with_context(|| format!("Failed to write '{}'", target.display()))?;
            copied += 1;
            counted();
        }
    }
    Ok(copied)
}

/// Replace the link at `path` with a copy of the file it resolves to, in one
/// rename. `false` when it resolves to nothing: the link is removed.
fn materialize(path: &Path) -> Res<bool> {
    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::remove_file(path).with_context(|| format!("Failed to remove the link '{}'", path.display()))?;
            return Ok(false);
        }
        Err(e) => return Err(e).with_context(|| format!("Failed to stat '{}'", path.display())),
    };
    if !meta.is_file() {
        bail!("'{}' links to something that is not a file; cactup only links files", path.display());
    }
    let dir = path.parent().expect("a link in a tree has a parent");
    let temp = tempfile::Builder::new()
        .prefix(".cactup-reify")
        .tempfile_in(dir)
        .with_context(|| format!("Failed to create a file in '{}'", dir.display()))?;
    fs::copy(path, temp.path()).with_context(|| format!("Failed to copy '{}'", path.display()))?;
    crate::treecopy::set_times(temp.path(), &meta);
    temp.persist(path).with_context(|| format!("Failed to replace the link '{}'", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    #[test]
    fn reify_turns_copy_links_into_copies_and_leaves_other_links() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("a");
        let copy = tmp.path().join("b");
        fs::create_dir_all(source.join("repo/bin")).unwrap();
        fs::create_dir_all(copy.join("repo/bin")).unwrap();
        fs::write(source.join("repo/file"), "content").unwrap();
        fs::write(source.join("repo/bin/run"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(source.join("repo/bin/run"), fs::Permissions::from_mode(0o755)).unwrap();
        symlink(source.join("repo/file"), copy.join("repo/file")).unwrap();
        symlink(source.join("repo/bin/run"), copy.join("repo/bin/run")).unwrap();
        symlink(source.join("repo/deleted"), copy.join("repo/deleted")).unwrap();
        symlink("file", copy.join("repo/relative")).unwrap();
        symlink("/usr/bin/env", copy.join("repo/elsewhere")).unwrap();

        let cow = Cow::linked_to(Some(source.clone()));
        assert!(cow.is_link(&copy.join("repo/file")));
        assert!(!cow.is_link(&copy.join("repo/relative")));
        assert!(!Cow::default().is_link(&copy.join("repo/file")));

        let count = std::sync::atomic::AtomicUsize::new(0);
        let counted = || {
            count.fetch_add(1, Ordering::Relaxed);
        };
        let done = cow.reify(&copy.join("repo"), &AtomicBool::new(false), &counted).unwrap();
        assert_eq!(done, Reified { copied: 2, dangling: 1 });
        assert_eq!(count.load(Ordering::Relaxed), 3);

        let file = copy.join("repo/file");
        assert!(!fs::symlink_metadata(&file).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_to_string(&file).unwrap(), "content");
        assert_eq!(fs::metadata(copy.join("repo/bin/run")).unwrap().permissions().mode() & 0o777, 0o755);
        assert_eq!(
            fs::metadata(&file).unwrap().modified().unwrap(),
            fs::metadata(source.join("repo/file")).unwrap().modified().unwrap()
        );
        assert!(fs::symlink_metadata(copy.join("repo/deleted")).is_err());
        assert_eq!(fs::read_link(copy.join("repo/relative")).unwrap(), Path::new("file"));
        assert_eq!(fs::read_link(copy.join("repo/elsewhere")).unwrap(), Path::new("/usr/bin/env"));

        // The source is untouched, and writing the copy's file now leaves it so.
        fs::write(&file, "changed").unwrap();
        assert_eq!(fs::read_to_string(source.join("repo/file")).unwrap(), "content");
        // Nothing is left behind in the directory.
        let mut left: Vec<_> = fs::read_dir(copy.join("repo")).unwrap().map(|e| e.unwrap().file_name()).collect();
        left.sort();
        assert_eq!(left, ["bin", "elsewhere", "file", "relative"]);
    }

    #[test]
    fn before_replace_removes_only_a_copy_link() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("a");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("p.par"), "source").unwrap();
        let mine = tmp.path().join("mine");
        fs::create_dir_all(&mine).unwrap();
        symlink(source.join("p.par"), mine.join("p.par")).unwrap();
        symlink("/usr/bin/env", mine.join("other")).unwrap();

        let cow = Cow::linked_to(Some(source.clone()));
        cow.before_replace(&mine.join("p.par")).unwrap();
        cow.before_replace(&mine.join("other")).unwrap();
        cow.before_replace(&mine.join("absent")).unwrap();
        fs::write(mine.join("p.par"), "mine").unwrap();
        assert_eq!(fs::read_to_string(source.join("p.par")).unwrap(), "source");
        assert!(fs::symlink_metadata(mine.join("other")).unwrap().file_type().is_symlink());
    }

    #[test]
    fn reify_stops_when_asked() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("a");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("f"), "x").unwrap();
        let copy = tmp.path().join("b");
        fs::create_dir_all(&copy).unwrap();
        symlink(source.join("f"), copy.join("f")).unwrap();
        let err = Cow::linked_to(Some(source)).reify(&copy, &AtomicBool::new(true), &|| ()).unwrap_err();
        assert!(format!("{err:#}").contains("Interrupted"));
        assert!(fs::symlink_metadata(copy.join("f")).unwrap().file_type().is_symlink());
    }
}
