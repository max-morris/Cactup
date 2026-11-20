//! `cactup installation delete <alias>`: remove an installation (spec §3.1).
//!
//! Only what cactup put in the installation home is deleted: the Cactus tree,
//! `.cactup/`, the pristine thornlist and `imports/`. The home itself goes only
//! once that leaves it empty: `install --install-prefix` names the directory
//! the tree is created *in*, which may be one that holds the user's other work
//! (`~/src`), or the install-home that holds the shared build cache.

use super::{prompt_with_default, remove_tree_until, Ctx};
use crate::installation::Installation;
use crate::Res;
use anyhow::{anyhow, bail};
use colored::Colorize;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

pub fn dispatch(ctx: &Ctx, alias: String, force: bool) -> Res<()> {
    let db = ctx.db.read()?;
    let Some(entry) = db.installations.get(&alias) else {
        bail!("No installation named \"{alias}\" (see `cactup list`)");
    };
    let inst = Installation::new(&alias, &entry.path);
    // Not even with -f: their files are links into this tree.
    let dependents = super::copy::dependents(&db, &inst.root);
    drop(db);
    if !dependents.is_empty() {
        bail!("{alias} has symbolic copies: {}; reify or delete them first", dependents.join(", "));
    }

    let home_exists = inst.root.is_dir();
    // Read before anything is deleted: the recorded root-dir is in
    // `.cactup/installation.toml`.
    let cactus_root = match home_exists {
        true => Some(inst.cactus_root()?),
        false => None,
    };

    if let Some(cactus_root) = &cactus_root {
        let in_flight = builds_in_flight(cactus_root);
        if !in_flight.is_empty() && !force {
            bail!(
                "{}; stop it with `cactup --installation {alias} build stop <config>` first, \
                 or pass -f to delete anyway",
                in_flight.join("; ")
            );
        }
    }

    if !force {
        // Best effort: the registries only feed the warning.
        let sims = inst.simulations().map(|r| r.simulations.len()).unwrap_or(0);
        let tests = inst.tests().map(|r| r.tests.len()).unwrap_or(0);
        let kept = match (sims, tests) {
            (0, 0) => "Simulation and test output is kept.".to_owned(),
            _ => format!(
                "Its {sims} simulation(s) and {tests} test run(s) keep their output, but cactup \
                 will no longer list them."
            ),
        };
        let answer = prompt_with_default(
            &format!(
                "Really delete installation {} and its source tree in '{}'? {kept} \
                 Type \"yes\" to confirm",
                alias.bold(),
                inst.root.display()
            ),
            "no",
        )?;
        if !answer.eq_ignore_ascii_case("yes") {
            println!("Aborted.");
            return Ok(());
        }
    }

    let mut kept_home = false;
    let mut removed_link = None;
    if let Some(cactus_root) = &cactus_root {
        delete_with_progress(&inst, &alias)?;
        kept_home = !remove_home_if_empty(&inst.root)?;
        removed_link = remove_default_symlink(cactus_root)?;
    } else {
        println!(
            "{} '{}' no longer exists; removing the registration only",
            "note:".yellow(),
            inst.root.display()
        );
    }

    // Field-scoped RMW (§2.3): drop the entry and, if it was active, the
    // active pointer — never silently promote another installation.
    let was_active = ctx.db.update(|database| {
        database.installations.shift_remove(&alias);
        Ok(if database.active_installation.as_deref() == Some(&alias) {
            database.active_installation = None;
            true
        } else {
            false
        })
    })?;

    println!("{}", format!("Deleted installation {}.", alias.bold()).bright_green());
    if let Some(link) = removed_link {
        println!("Removed the symlink '{}'.", link.display());
    }
    if kept_home {
        println!(
            "Kept '{}': it holds files cactup did not put there.",
            inst.root.display()
        );
    }
    if was_active {
        println!(
            "It was the active installation; pick a new one with `{}`.",
            "cactup use <alias>".bold()
        );
    }
    Ok(())
}

/// One line per in-flight build among the installation's configs (a build
/// writes into the tree being deleted until it finishes).
fn builds_in_flight(cactus_root: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(cactus_root.join("configs")) else { return Vec::new() };
    let mut phrases: Vec<String> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            super::build::in_flight_build(&e.path(), &name, None)
        })
        .collect();
    phrases.sort();
    phrases
}

/// What cactup put in the installation home, in deletion order, those that
/// exist. `.cactup/` comes last: it records the root-dir, so a deletion cut
/// short anywhere before it is finished by running the same command again.
fn targets(inst: &Installation) -> Res<Vec<PathBuf>> {
    let candidates = [
        inst.cactus_root()?,
        inst.root.join("imports"),
        inst.source_thornlist(),
        inst.legacy_source_thornlist(),
        inst.cactup_dir(),
    ];
    Ok(candidates.into_iter().filter(|p| fs::symlink_metadata(p).is_ok()).collect())
}

/// Delete [`targets`], counting files on one progress line (a whole tree is
/// one unlink per file: minutes on NFS), and stopping on Ctrl-C.
fn delete_with_progress(inst: &Installation, alias: &str) -> Res<()> {
    use prodash::{Count, Progress};
    let (progress, renderer) = crate::manifest::setup_prodash_if_tty();
    let layout = crate::progress::Layout::for_names([alias]);
    let mut line = crate::progress::Line::counting(progress.add_child(alias), alias, layout);
    line.phase("deleting");
    line.init(None, Some(prodash::unit::label("files")));
    let result = delete_until(inst, &gix::interrupt::IS_INTERRUPTED, &|| line.inc());
    drop(line);
    if let Some(renderer) = renderer {
        renderer.shutdown_and_wait();
    }
    result
}

fn delete_until(inst: &Installation, stop: &AtomicBool, removed: &dyn Fn()) -> Res<()> {
    for target in targets(inst)? {
        remove_tree_until(&target, stop, removed).map_err(|e| match stop.load(Ordering::Relaxed) {
            true => anyhow!(
                "Interrupted; installation {} is partly deleted and still registered: run \
                 `cactup inst delete {}` again to finish",
                inst.alias,
                inst.alias
            ),
            false => e,
        })?;
    }
    Ok(())
}

/// Remove the installation home if nothing is left in it. `false` when it
/// holds something else and was kept.
fn remove_home_if_empty(home: &Path) -> Res<bool> {
    let mut entries = fs::read_dir(home).map_err(|e| anyhow!("Failed to list '{}': {e}", home.display()))?;
    if entries.next().is_some() {
        return Ok(false);
    }
    fs::remove_dir(home).map_err(|e| anyhow!("Failed to remove '{}': {e}", home.display()))?;
    Ok(true)
}

/// `install`'s symlink is not recorded, so only the one at its default place
/// (`~/<root-dir's last component>`) is removed, and only while it still
/// points at the deleted tree. Returns the link removed.
fn remove_default_symlink(cactus_root: &Path) -> Res<Option<PathBuf>> {
    let Some(home) = std::env::home_dir() else { return Ok(None) };
    remove_symlink_to(&home, cactus_root)
}

fn remove_symlink_to(dir: &Path, cactus_root: &Path) -> Res<Option<PathBuf>> {
    let Some(name) = cactus_root.file_name() else { return Ok(None) };
    let link = dir.join(name);
    let points_here = fs::symlink_metadata(&link).is_ok_and(|m| m.file_type().is_symlink())
        && fs::read_link(&link).is_ok_and(|target| target == cactus_root);
    if !points_here {
        return Ok(None);
    }
    fs::remove_file(&link).map_err(|e| anyhow!("Failed to remove the symlink '{}': {e}", link.display()))?;
    Ok(Some(link))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An installation home as `install` leaves it, with a custom root-dir
    /// (so a rerun after an interruption must still find it) and a file of
    /// the user's beside it.
    fn fixture(home: &Path) -> Installation {
        let inst = Installation::new("et", home);
        let meta = crate::installation::InstallationMeta {
            root_dir: Some("MyTree".to_owned()),
            ..Default::default()
        };
        fs::create_dir_all(inst.cactup_dir()).unwrap();
        inst.locked().unwrap().set_meta(&meta).unwrap();
        let tree = home.join("MyTree");
        fs::create_dir_all(tree.join("repos/a/.git")).unwrap();
        fs::write(tree.join("repos/a/file"), "x").unwrap();
        fs::write(tree.join("repos/a/.git/HEAD"), "ref").unwrap();
        fs::create_dir_all(home.join("imports")).unwrap();
        fs::write(home.join("imports/p.par"), "").unwrap();
        fs::write(inst.source_thornlist(), "").unwrap();
        inst
    }

    #[test]
    fn only_what_cactup_put_there_is_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("src");
        fs::create_dir_all(&home).unwrap();
        let inst = fixture(&home);
        fs::write(home.join("thesis.tex"), "mine").unwrap();
        fs::create_dir_all(home.join(".cactup-build-cache")).unwrap();

        let count = std::cell::Cell::new(0);
        delete_until(&inst, &AtomicBool::new(false), &|| count.set(count.get() + 1)).unwrap();
        assert!(count.get() > 5, "each removed entry is counted");

        let mut left: Vec<_> = fs::read_dir(&home).unwrap().map(|e| e.unwrap().file_name()).collect();
        left.sort();
        assert_eq!(left, [".cactup-build-cache", "thesis.tex"]);
        assert!(!remove_home_if_empty(&home).unwrap(), "a home holding the user's files is kept");
        assert!(home.is_dir());
    }

    #[test]
    fn an_empty_home_is_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("et");
        fs::create_dir_all(&home).unwrap();
        let inst = fixture(&home);
        delete_until(&inst, &AtomicBool::new(false), &|| ()).unwrap();
        assert!(remove_home_if_empty(&home).unwrap());
        assert!(!home.exists());
    }

    #[test]
    fn an_interrupted_delete_says_so_and_can_be_finished() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("et");
        fs::create_dir_all(&home).unwrap();
        let inst = fixture(&home);

        let err = delete_until(&inst, &AtomicBool::new(true), &|| ()).unwrap_err();
        assert!(format!("{err:#}").contains("cactup inst delete et"), "{err:#}");
        assert!(inst.cactup_dir().join("installation.toml").is_file(), "the root-dir record survives");

        // A fresh handle, as the rerun has: it still finds the custom root.
        let again = Installation::new("et", &home);
        assert_eq!(again.cactus_root().unwrap(), home.join("MyTree"));
        delete_until(&again, &AtomicBool::new(false), &|| ()).unwrap();
        assert!(remove_home_if_empty(&home).unwrap());
    }

    #[test]
    fn only_a_symlink_to_the_deleted_tree_is_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let tree = tmp.path().join("inst/Cactus");
        let links = tmp.path().join("home");
        fs::create_dir_all(&links).unwrap();

        std::os::unix::fs::symlink(tmp.path().join("other/Cactus"), links.join("Cactus")).unwrap();
        assert_eq!(remove_symlink_to(&links, &tree).unwrap(), None, "points elsewhere: kept");
        assert!(fs::symlink_metadata(links.join("Cactus")).is_ok());

        fs::remove_file(links.join("Cactus")).unwrap();
        std::os::unix::fs::symlink(&tree, links.join("Cactus")).unwrap();
        assert_eq!(remove_symlink_to(&links, &tree).unwrap(), Some(links.join("Cactus")));
        assert!(fs::symlink_metadata(links.join("Cactus")).is_err());

        fs::create_dir_all(links.join("Cactus")).unwrap();
        assert_eq!(remove_symlink_to(&links, &tree).unwrap(), None, "a real directory is never touched");
        assert!(links.join("Cactus").is_dir());
    }
}
