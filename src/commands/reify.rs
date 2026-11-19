//! `cactup installation reify <alias> [PATH]...`: make a symbolic copy's
//! files its own (spec §3.1).
//!
//! Every copy link (`crate::cow`) under the given paths, or in the whole
//! installation, becomes a copy of the file it points at; a link to a file
//! the source no longer has is removed. Reifying the whole installation ends
//! its dependence: the record's `linked-to` goes, and the source may then be
//! deleted. Interrupted, every file done so far stays done, and the same
//! command finishes the job.

use super::copy::lock_fetches;
use super::Ctx;
use crate::cow::{Cow, Reified};
use crate::installation::{Installation, LEGACY_THORNLIST, SOURCE_THORNLIST};
use crate::lock::LinkLock;
use crate::Res;
use anyhow::{anyhow, bail, Context};
use colored::Colorize;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

pub fn dispatch(ctx: &Ctx, alias: &str, paths: &[PathBuf]) -> Res<()> {
    let db = ctx.db.read()?;
    let Some(entry) = db.installations.get(alias).cloned() else {
        bail!("No installation named \"{alias}\" (see `cactup list`)");
    };
    drop(db);
    let inst = Installation::new(alias, &entry.path);
    let Some(source) = inst.meta()?.linked_to else {
        println!("{alias} is not a symbolic copy: its files are its own already.");
        return Ok(());
    };
    let source_name = entry.copied_from.clone().unwrap_or_else(|| source.display().to_string());

    // Nothing may rewrite either tree meanwhile: a refetch here would write
    // through links not yet replaced, one there would change what is copied.
    fs::create_dir_all(inst.cactup_dir())
        .with_context(|| format!("Failed to create '{}'", inst.cactup_dir().display()))?;
    let _own = LinkLock::acquire(&inst.fetch_lock_path())?.with_heartbeat();
    let _sources = lock_fetches(&Installation::new("", &source))?;

    let units = match paths {
        [] => whole(&inst)?,
        paths => paths.iter().map(|p| inside(&inst, p)).collect::<Res<Vec<_>>>()?,
    };
    let cow = Cow::linked_to(Some(source));
    let done = reify_with_progress(&cow, &units, alias)?;

    if paths.is_empty() {
        let locked = inst.locked()?;
        let mut meta = locked.meta()?;
        meta.linked_to = None;
        locked.set_meta(&meta)?;
        println!(
            "{}",
            format!("{alias} no longer depends on {source_name}: made {} file(s) its own.", done.copied).bright_green()
        );
    } else {
        println!(
            "{}",
            format!("Made {} file(s) {alias}'s own; it still reads {source_name}'s other files.", done.copied)
                .bright_green()
        );
    }
    if done.dangling > 0 {
        println!("Removed {} link(s) to files {source_name} no longer has.", done.dangling);
    }
    Ok(())
}

/// Everything a symbolic copy made of links: the Cactus tree, the repos one
/// by one (the work is spread over them), `imports/` and the pristine
/// thornlist. Nothing else in the installation directory is cactup's.
fn whole(inst: &Installation) -> Res<Vec<PathBuf>> {
    let tree = inst.cactus_root()?;
    let mut units = Vec::new();
    if let Ok(entries) = fs::read_dir(&tree) {
        for entry in entries {
            let entry = entry.with_context(|| format!("Failed to list '{}'", tree.display()))?;
            let path = entry.path();
            match entry.file_name() == "repos" && entry.file_type().is_ok_and(|t| t.is_dir()) {
                true => {
                    for repo in fs::read_dir(&path).with_context(|| format!("Failed to list '{}'", path.display()))? {
                        units.push(repo.with_context(|| format!("Failed to list '{}'", path.display()))?.path());
                    }
                }
                false => units.push(path),
            }
        }
    }
    units.extend([inst.root.join("imports"), inst.root.join(SOURCE_THORNLIST), inst.root.join(LEGACY_THORNLIST)]);
    units.sort();
    Ok(units)
}

/// `path` as the user gave it: absolute, or relative to the current
/// directory, or else to the Cactus tree; it must be in the installation.
fn inside(inst: &Installation, path: &Path) -> Res<PathBuf> {
    let exists = |p: &Path| fs::symlink_metadata(p).is_ok();
    let given = if path.is_absolute() {
        path.to_owned()
    } else {
        let here = std::env::current_dir().context("Failed to read the current directory")?.join(path);
        let in_tree = inst.cactus_root()?.join(path);
        if !exists(&here) && exists(&in_tree) { in_tree } else { here }
    };
    if !exists(&given) {
        bail!("'{}' does not exist", given.display());
    }
    // Resolved up to the last part, which may itself be one of the links.
    let parent = given.parent().ok_or_else(|| anyhow!("'{}' is not in installation {}", given.display(), inst.alias))?;
    let resolved = fs::canonicalize(parent)
        .with_context(|| format!("Failed to resolve '{}'", parent.display()))?
        .join(given.file_name().unwrap_or_default());
    let home = fs::canonicalize(&inst.root).with_context(|| format!("Failed to resolve '{}'", inst.root.display()))?;
    if !resolved.starts_with(&home) || resolved == home {
        bail!("'{}' is not in installation {}", given.display(), inst.alias);
    }
    Ok(resolved)
}

/// [`reify_until`] on one progress line counting files, stopping on Ctrl-C.
fn reify_with_progress(cow: &Cow, units: &[PathBuf], alias: &str) -> Res<Reified> {
    use prodash::{Count, Progress};
    let (progress, renderer) = crate::manifest::setup_prodash_if_tty();
    let layout = crate::progress::Layout::for_names([alias]);
    let mut line = crate::progress::Line::counting(progress.add_child(alias), alias, layout);
    line.phase("reifying");
    line.init(None, Some(prodash::unit::label("files")));
    let result = reify_until(cow, units, &gix::interrupt::IS_INTERRUPTED, &|| line.inc());
    drop(line);
    if let Some(renderer) = renderer {
        renderer.shutdown_and_wait();
    }
    result.map_err(|e| match gix::interrupt::is_triggered() {
        true => anyhow!("Interrupted; the files done so far stay done: run the same command again to finish"),
        false => e,
    })
}

/// Reify `units`, several at a time.
fn reify_until(cow: &Cow, units: &[PathBuf], stop: &AtomicBool, counted: &(dyn Fn() + Sync)) -> Res<Reified> {
    let mut total = Reified::default();
    for done in crate::par::parallel_map(units, |unit| cow.reify(unit, stop, counted))? {
        total += done?;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn whole_names_the_repos_one_by_one_and_what_beside_the_tree_is_cactups() {
        let tmp = tempfile::tempdir().unwrap();
        let inst = Installation::new("b", tmp.path());
        fs::create_dir_all(tmp.path().join("Cactus/repos/flesh")).unwrap();
        fs::create_dir_all(tmp.path().join("Cactus/repos/cactusbase")).unwrap();
        fs::create_dir_all(tmp.path().join("Cactus/arrangements")).unwrap();
        fs::write(tmp.path().join("mine.txt"), "").unwrap();
        let rel = |units: Vec<PathBuf>| -> Vec<String> {
            units.iter().map(|u| u.strip_prefix(tmp.path()).unwrap().display().to_string()).collect()
        };
        assert_eq!(
            rel(whole(&inst).unwrap()),
            [
                "Cactus/arrangements",
                "Cactus/repos/cactusbase",
                "Cactus/repos/flesh",
                "einsteintoolkit.th",
                "imports",
                "installation-source.th"
            ]
        );
    }

    #[test]
    fn a_path_outside_the_installation_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("b");
        let inst = Installation::new("b", &home);
        fs::create_dir_all(home.join("Cactus/repos/flesh")).unwrap();
        fs::create_dir_all(tmp.path().join("elsewhere")).unwrap();
        let home = fs::canonicalize(&home).unwrap();
        assert_eq!(inside(&inst, Path::new("repos/flesh")).unwrap(), home.join("Cactus/repos/flesh"));
        let err = inside(&inst, &tmp.path().join("elsewhere")).unwrap_err();
        assert!(format!("{err:#}").contains("is not in installation b"), "{err:#}");
        let err = inside(&inst, &home).unwrap_err();
        assert!(format!("{err:#}").contains("is not in installation b"), "{err:#}");
        // A link into the installation is resolved up to its last part only.
        symlink(home.join("Cactus"), tmp.path().join("shortcut")).unwrap();
        assert_eq!(inside(&inst, &tmp.path().join("shortcut/repos")).unwrap(), home.join("Cactus/repos"));
    }
}
