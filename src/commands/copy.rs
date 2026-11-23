//! `cactup installation cp <alias> <new-name>`: copy an installation to a
//! new one (spec §3.1).
//!
//! What is copied is what makes the installation: the Cactus tree (its
//! repos, their history, the live thornlist and anything else the user keeps
//! there), `imports/`, the pristine thornlist, and from `.cactup/` the
//! installation record and the fetch state. Configs and builds stay behind:
//! a build records absolute paths into the tree it was built from, so a
//! copied one would go on reading the source's files. Simulations and test
//! runs stay listed under the source; the copy gets its own sim-home and
//! test-home, for its own alias.
//!
//! The copy is made in a staging directory inside the destination and moved
//! into place only once complete, and registered last: an interrupted copy
//! is never an installation, and running the same command again starts over.
//!
//! `--symbolic` makes the same directories, but every file of the tree and of
//! `imports/` an absolute link to the source's file (`crate::cow`). What the
//! copy's own commands write goes through copy-on-write; what cactup keeps
//! about the installation is copied all the same: `.cactup/`, the pristine
//! thornlist, and the live thornlist, which users edit by hand (an editor
//! writes through a link). The record names the source (`linked-to`), which
//! must then outlive the copy.

use super::Ctx;
use crate::cow::Cow;
use crate::installation::{Installation, LEGACY_THORNLIST, LIVE_THORNLIST, SOURCE_THORNLIST};
use crate::lock::{HeartbeatLock, LinkLock};
use crate::treecopy::{Copier, Mode};
use crate::Res;
use anyhow::{anyhow, bail, Context};
use colored::Colorize;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

/// What is not copied from the top of the Cactus tree: build products.
const NOT_COPIED: &[&str] = &["configs", "exe"];

/// What is copied from the source's `.cactup/`. The simulation and test
/// registries are not: those runs belong to the source.
const STATE_COPIED: &[&str] = &["installation.toml", "fetch-state.toml", "thornlists"];

pub fn dispatch(ctx: &Ctx, source: &str, new_name: &str, install_prefix: Option<&str>, symbolic: bool) -> Res<()> {
    if !super::install::valid_alias(new_name) {
        bail!("\"{new_name}\" is not a valid alias: use only letters, digits, '-', '_', and '.'");
    }
    let db = ctx.db.read()?;
    let Some(entry) = db.installations.get(source).cloned() else {
        bail!("No installation named \"{source}\" (see `cactup list`)");
    };
    if db.installations.contains_key(new_name) {
        bail!("An installation named \"{new_name}\" already exists");
    }
    let from = Installation::new(source, &entry.path);
    if !from.root.is_dir() {
        bail!("The directory of installation {source}, '{}', no longer exists", from.root.display());
    }
    let machine = super::machine::resolve(ctx)?;
    let dest = match install_prefix {
        Some(prefix) => PathBuf::from(crate::shell::expand_path(prefix)),
        None => super::install::install_home(&machine, &db).join(new_name),
    };
    drop(db);

    // Nothing may rewrite the repos while they are read; a refetch waits.
    let _locks = lock_fetches(&from)?;

    fs::create_dir_all(&dest).with_context(|| format!("Failed to create '{}'", dest.display()))?;
    let dest = fs::canonicalize(&dest).with_context(|| format!("Failed to resolve '{}'", dest.display()))?;
    let plan = Plan::new(&from, &dest, new_name, symbolic)?;
    let copied = copy_with_progress(&plan, new_name)?;

    let to = Installation::new(new_name, &dest);
    // The record came over as the source's: the active config is the
    // source's, the homes are named for its alias, and a symbolic source's
    // link is its own. Cleared, then resolved for this alias as `install`
    // does.
    {
        let locked = to.locked()?;
        let mut meta = locked.meta()?;
        meta.active_config = None;
        meta.sim_home = None;
        meta.test_home = None;
        meta.linked_to = symbolic.then(|| plan.from_home.clone());
        locked.set_meta(&meta)?;
    }
    to.ensure_meta(&machine, &ctx.db.read()?, None)?;

    ctx.db.update(|database| {
        if database.installations.contains_key(new_name) {
            bail!(
                "An installation named {new_name} appeared during the copy; '{}' was not registered",
                dest.display()
            );
        }
        let mut copy = entry.clone();
        copy.alias = new_name.to_owned();
        copy.path = dest.to_string_lossy().into_owned();
        copy.copied_from = Some(source.to_owned());
        database.installations.insert(new_name.to_owned(), copy);
        Ok(())
    })?;

    let (made, to_as) = if symbolic { ("Made a symbolic copy of", "as") } else { ("Copied", "to") };
    println!(
        "{}",
        format!(
            "{made} installation {} {to_as} {} in '{}'.",
            source.bold(),
            new_name.bold(),
            to.cactus_root()?.display()
        )
        .bright_green()
    );
    if !copied.skipped.is_empty() {
        let shown: Vec<String> = copied.skipped.iter().take(3).map(|p| format!("'{}'", p.display())).collect();
        let more = copied.skipped.len().saturating_sub(shown.len());
        let more = if more > 0 { format!(" and {more} more") } else { String::new() };
        println!("{}", format!("Skipped special files: {}{more}.", shown.join(", ")).yellow());
    }
    println!("Switch to it with `{}`.", format!("cactup use {new_name}").bold());
    if symbolic {
        println!("Convert to real files with `{}`.", format!("cactup inst reify {new_name}").bold());
    }
    Ok(())
}

/// Hold the fetch lock of `from`, and of every installation its files link
/// into, for as long as their files are read.
pub(crate) fn lock_fetches(from: &Installation) -> Res<Vec<HeartbeatLock>> {
    let mut locks = Vec::new();
    let mut home = Some(from.root.clone());
    while let Some(here) = home {
        let inst = Installation::new("", &here);
        fs::create_dir_all(inst.cactup_dir())
            .with_context(|| format!("Failed to create '{}'", inst.cactup_dir().display()))?;
        locks.push(LinkLock::acquire(&inst.fetch_lock_path())?.with_heartbeat());
        home = inst.meta()?.linked_to;
        if locks.len() > 40 {
            bail!("'{}' is a symbolic copy of a symbolic copy, 40 deep; something is wrong", from.root.display());
        }
    }
    Ok(locks)
}

/// For a command that writes in `inst`: when it is a symbolic copy, hold the
/// fetch locks of the installations it reads (as [`lock_fetches`]), and say
/// when it has symbolic copies of its own, which the changes reach.
pub(crate) fn lock_sources(ctx: &Ctx, inst: &Installation) -> Res<Vec<HeartbeatLock>> {
    note_dependents(ctx, inst)?;
    match Cow::at(&inst.root)?.source() {
        Some(source) => lock_fetches(&Installation::new("", source)),
        None => Ok(Vec::new()),
    }
}

/// Say, before a command changes `inst`'s files, which symbolic copies read
/// them and so see the change too.
pub(crate) fn note_dependents(ctx: &Ctx, inst: &Installation) -> Res<()> {
    let dependents = dependents(&ctx.db.read()?, &inst.root);
    if !dependents.is_empty() {
        let copies = if dependents.len() == 1 { "copy" } else { "copies" };
        println!("{} this also changes symbolic {copies} {}", "note:".yellow(), dependents.join(", "));
    }
    Ok(())
}

/// The installations whose files link into the installation home `home`:
/// its symbolic copies (`linked-to`), in registry order.
pub(crate) fn dependents(db: &crate::database::Database, home: &Path) -> Vec<String> {
    let home = fs::canonicalize(home).unwrap_or_else(|_| home.to_owned());
    db.installations
        .values()
        .filter(|entry| entry.copied_from.is_some())
        .filter(|entry| {
            // Unreadable: not provably one, and the command it is asked for
            // reads that record again and says what is wrong with it.
            Installation::new(&entry.alias, &entry.path).meta().ok().and_then(|m| m.linked_to).is_some_and(|to| to == home)
        })
        .map(|entry| entry.alias.clone())
        .collect()
}

/// Where everything goes. Paths under the homes are relative to them.
struct Plan {
    from_home: PathBuf,
    /// The source's own copy links, when it is a symbolic copy.
    source: Cow,
    /// How the tree's files and `imports/` are copied.
    mode: Mode,
    dest: PathBuf,
    /// Inside `dest`: the copy is made here, then moved into place.
    staging: PathBuf,
    /// The Cactus tree, relative to the homes (the recorded root-dir).
    root_dir: PathBuf,
    /// What moves from `staging` to `dest` once the copy is complete, in
    /// that order.
    placed: Vec<PathBuf>,
}

impl Plan {
    fn new(from: &Installation, dest: &Path, new_name: &str, symbolic: bool) -> Res<Plan> {
        let from_home = fs::canonicalize(&from.root)
            .with_context(|| format!("Failed to resolve '{}'", from.root.display()))?;
        if dest.starts_with(&from_home) {
            bail!(
                "'{}' is inside installation {}; choose another location with --install-prefix",
                dest.display(),
                from.alias
            );
        }
        let root_dir = from
            .cactus_root()?
            .strip_prefix(&from.root)
            .map(Path::to_path_buf)
            .map_err(|_| anyhow!("The source tree of installation {} is outside its directory", from.alias))?;
        let mut placed = vec![root_dir.clone(), PathBuf::from("imports")];
        placed.extend([SOURCE_THORNLIST, LEGACY_THORNLIST].map(PathBuf::from));
        placed.retain(|rel| fs::symlink_metadata(from_home.join(rel)).is_ok());
        // Last: the record is what makes the directory an installation.
        placed.push(PathBuf::from(".cactup"));
        for rel in &placed {
            let there = dest.join(rel);
            if fs::symlink_metadata(&there).is_ok() {
                bail!("'{}' already exists; choose another location with --install-prefix", there.display());
            }
        }
        let staging = dest.join(format!(".cactup-cp-{new_name}.partial"));
        let source = Cow::linked_to(from.meta()?.linked_to);
        let mode = if symbolic { Mode::Link } else { Mode::Copy };
        Ok(Plan { from_home, source, mode, dest: dest.to_owned(), staging, root_dir, placed })
    }
}

struct Copied {
    skipped: Vec<PathBuf>,
}

/// [`copy_until`] on one progress line counting files, stopping on Ctrl-C.
fn copy_with_progress(plan: &Plan, alias: &str) -> Res<Copied> {
    use prodash::{Count, Progress};
    let (progress, renderer) = crate::manifest::setup_prodash_if_tty();
    let layout = crate::progress::Layout::for_names([alias]);
    let mut line = crate::progress::Line::counting(progress.add_child(alias), alias, layout);
    line.phase("copying");
    line.init(None, Some(prodash::unit::label("files")));
    let result = copy_until(plan, &gix::interrupt::IS_INTERRUPTED, &|| line.inc());
    drop(line);
    if let Some(renderer) = renderer {
        renderer.shutdown_and_wait();
    }
    result
}

/// Copy into the staging directory, the repos in parallel, then move the
/// result into place.
fn copy_until(plan: &Plan, stop: &AtomicBool, counted: &(dyn Fn() + Sync)) -> Res<Copied> {
    if fs::symlink_metadata(&plan.staging).is_ok() {
        // What an interrupted run left: start over.
        super::remove_tree_until(&plan.staging, stop, &|| ())?;
    }
    let copier = Copier::new(&plan.from_home, &plan.dest, stop, counted).with_source(plan.source.clone());
    let units = stage(plan, &copier)?;
    let results = crate::par::parallel_map(&units, |(from, to, mode)| copier.copy(from, to, *mode));
    let failed = match results {
        Ok(results) => results.into_iter().find_map(Result::err),
        Err(e) => Some(e),
    };
    let failed = failed.or_else(|| own_live_thornlists(plan).err());
    let failed = failed.or_else(|| finish_staged(plan, &copier).err());
    if let Some(e) = failed {
        let left = format!("nothing was registered; rerun to clean up '{}'", plan.staging.display());
        return Err(match stop.load(std::sync::atomic::Ordering::Relaxed) {
            true => anyhow!("Interrupted; {left}"),
            false => e.context(format!("The copy failed; {left}")),
        });
    }
    for rel in &plan.placed {
        let (staged, there) = (plan.staging.join(rel), plan.dest.join(rel));
        if let Some(parent) = there.parent() {
            fs::create_dir_all(parent).with_context(|| format!("Failed to create '{}'", parent.display()))?;
        }
        fs::rename(&staged, &there)
            .with_context(|| format!("Failed to move '{}' to '{}'", staged.display(), there.display()))?;
    }
    // Only the directories above a nested root-dir are left.
    fs::remove_dir_all(&plan.staging).with_context(|| format!("Failed to remove '{}'", plan.staging.display()))?;
    Ok(Copied { skipped: copier.skipped() })
}

/// A symbolic copy's live thornlist is its own from the start: it is edited
/// by hand, and an editor writes through a link.
fn own_live_thornlists(plan: &Plan) -> Res<()> {
    if plan.mode != Mode::Link {
        return Ok(());
    }
    let thornlists = plan.staging.join(&plan.root_dir).join("thornlists");
    let links = Cow::linked_to(Some(plan.from_home.clone()));
    for name in [LIVE_THORNLIST, LEGACY_THORNLIST] {
        links.reify_quietly(&thornlists.join(name))?;
    }
    Ok(())
}

/// One unit of copying: a source entry, where its copy is staged, and how
/// its files are copied.
type Unit = (PathBuf, PathBuf, Mode);

/// Make the staging skeleton and list the units of work: every repo on its
/// own, since one repo is what holds a worker the longest, and every other
/// entry of the tree.
fn stage(plan: &Plan, copier: &Copier) -> Res<Vec<Unit>> {
    let mut units = Vec::new();
    let from_tree = plan.from_home.join(&plan.root_dir);
    let staged_tree = plan.staging.join(&plan.root_dir);
    fs::create_dir_all(staged_tree.parent().expect("the root-dir is below the staging directory"))
        .with_context(|| format!("Failed to create '{}'", plan.staging.display()))?;
    if from_tree.is_dir() {
        copier.make_dir(&staged_tree)?;
        for entry in sorted_entries(&from_tree)? {
            let name = entry.file_name().expect("a directory entry has a name");
            if NOT_COPIED.iter().any(|n| name == *n) {
                continue;
            }
            let staged = staged_tree.join(name);
            let real_dir = fs::symlink_metadata(&entry).is_ok_and(|m| m.is_dir());
            if name == "repos" && real_dir {
                copier.make_dir(&staged)?;
                units.extend(sorted_entries(&entry)?.into_iter().map(|repo| {
                    let to = staged.join(repo.file_name().expect("a directory entry has a name"));
                    (repo, to, plan.mode)
                }));
            } else {
                units.push((entry, staged, plan.mode));
            }
        }
    }
    for rel in plan.placed.iter().filter(|rel| **rel != plan.root_dir && **rel != Path::new(".cactup")) {
        // The pristine thornlist is cactup's record of the installation.
        let mode = if *rel == Path::new("imports") { plan.mode } else { Mode::Copy };
        units.push((plan.from_home.join(rel), plan.staging.join(rel), mode));
    }
    let staged_state = plan.staging.join(".cactup");
    copier.make_dir(&staged_state)?;
    for name in STATE_COPIED {
        let from = plan.from_home.join(".cactup").join(name);
        if fs::symlink_metadata(&from).is_ok() {
            units.push((from, staged_state.join(name), Mode::Copy));
        }
    }
    Ok(units)
}

/// Give the directories [`stage`] made their source's mode and times, once
/// the units under them are done.
fn finish_staged(plan: &Plan, copier: &Copier) -> Res<()> {
    let from_tree = plan.from_home.join(&plan.root_dir);
    let staged_tree = plan.staging.join(&plan.root_dir);
    if staged_tree.join("repos").is_dir() {
        copier.finish_dir(&from_tree.join("repos"), &staged_tree.join("repos"))?;
    }
    if staged_tree.is_dir() {
        copier.finish_dir(&from_tree, &staged_tree)?;
    }
    if plan.from_home.join(".cactup").is_dir() {
        copier.finish_dir(&plan.from_home.join(".cactup"), &plan.staging.join(".cactup"))?;
    }
    Ok(())
}

fn sorted_entries(dir: &Path) -> Res<Vec<PathBuf>> {
    let mut entries = fs::read_dir(dir)
        .with_context(|| format!("Failed to list '{}'", dir.display()))?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("Failed to list '{}'", dir.display()))?;
    entries.sort();
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::installation::InstallationMeta;
    use std::os::unix::fs::symlink;

    /// An installation home as `install` leaves it, with a custom root-dir, a
    /// built config, simulations, and a file of the user's beside it.
    fn fixture(home: &Path) -> Installation {
        let inst = Installation::new("et", home);
        let meta = InstallationMeta {
            root_dir: Some("My/Tree".to_owned()),
            active_config: Some("sim".to_owned()),
            sim_home: Some("/sims/et".into()),
            ..Default::default()
        };
        fs::create_dir_all(inst.cactup_dir().join("thornlists")).unwrap();
        inst.locked().unwrap().set_meta(&meta).unwrap();
        fs::write(inst.cactup_dir().join("simulations.toml"), "[simulations]\n").unwrap();
        fs::write(inst.cactup_dir().join("fetch-state.toml"), "schema = 1\n").unwrap();
        fs::write(inst.cactup_dir().join("thornlists/20260101-000000.th"), "old\n").unwrap();
        let tree = home.join("My/Tree");
        for repo in ["flesh", "cactusbase"] {
            fs::create_dir_all(tree.join("repos").join(repo).join(".git")).unwrap();
            fs::write(tree.join("repos").join(repo).join(".git/HEAD"), "ref").unwrap();
            fs::write(tree.join("repos").join(repo).join("file"), repo).unwrap();
        }
        fs::create_dir_all(tree.join("arrangements/CactusBase")).unwrap();
        symlink("../../repos/cactusbase", tree.join("arrangements/CactusBase/Boundary")).unwrap();
        symlink("repos/flesh/file", tree.join("Makefile")).unwrap();
        fs::create_dir_all(tree.join("thornlists")).unwrap();
        fs::write(tree.join("thornlists/installation-default.th"), "live\n").unwrap();
        fs::create_dir_all(tree.join("configs/sim/build")).unwrap();
        fs::create_dir_all(tree.join("exe")).unwrap();
        fs::write(tree.join("notes.md"), "mine").unwrap();
        fs::create_dir_all(home.join("imports")).unwrap();
        fs::write(home.join("imports/p.par"), "").unwrap();
        fs::write(inst.source_thornlist(), "pristine\n").unwrap();
        fs::write(home.join("unrelated.txt"), "not cactup's").unwrap();
        inst
    }

    fn copy(from: &Installation, dest: &Path) -> Res<Copied> {
        fs::create_dir_all(dest).unwrap();
        let dest = fs::canonicalize(dest).unwrap();
        let plan = Plan::new(from, &dest, "copy", false)?;
        copy_until(&plan, &AtomicBool::new(false), &|| ())
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> =
            fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    #[test]
    fn the_installation_is_copied_without_builds_or_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let from = fixture(&tmp.path().join("et"));
        let dest = tmp.path().join("copy");
        copy(&from, &dest).unwrap();

        assert_eq!(names(&dest), [".cactup", "My", "imports", "installation-source.th"]);
        assert_eq!(names(&dest.join(".cactup")), ["fetch-state.toml", "installation.toml", "thornlists"]);
        let tree = dest.join("My/Tree");
        assert_eq!(names(&tree), ["Makefile", "arrangements", "notes.md", "repos", "thornlists"]);
        assert_eq!(names(&tree.join("repos")), ["cactusbase", "flesh"]);
        assert_eq!(fs::read_to_string(tree.join("Makefile")).unwrap(), "flesh");
        assert_eq!(fs::read_to_string(tree.join("repos/flesh/.git/HEAD")).unwrap(), "ref");
        assert_eq!(
            fs::read_link(tree.join("arrangements/CactusBase/Boundary")).unwrap(),
            Path::new("../../repos/cactusbase")
        );
        assert_eq!(fs::read_to_string(dest.join("imports/p.par")).unwrap(), "");

        // The record came over whole; `dispatch` rewrites what is the source's.
        let to = Installation::new("copy", &dest);
        let meta = to.meta().unwrap();
        assert_eq!(meta.root_dir.as_deref(), Some("My/Tree"));
        assert_eq!(to.cactus_root().unwrap(), tree);
    }

    #[test]
    fn existing_files_at_the_destination_are_never_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let from = fixture(&tmp.path().join("et"));
        let dest = tmp.path().join("copy");
        fs::create_dir_all(dest.join("imports")).unwrap();
        let err = copy(&from, &dest).err().expect("refused");
        assert!(format!("{err:#}").contains("already exists"), "{err:#}");
        assert_eq!(names(&dest), ["imports"]);

        // A home that holds other things is fine.
        let other = tmp.path().join("src");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("thesis.tex"), "").unwrap();
        copy(&from, &other).unwrap();
        assert!(other.join("My/Tree/repos/flesh/file").is_file());
        assert!(other.join("thesis.tex").is_file());
    }

    #[test]
    fn a_copy_inside_the_source_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let from = fixture(&tmp.path().join("et"));
        let err = copy(&from, &tmp.path().join("et/My/Tree/sub")).err().expect("refused");
        assert!(format!("{err:#}").contains("inside installation et"), "{err:#}");
    }

    #[test]
    fn an_interrupted_copy_leaves_nothing_in_place_and_is_started_over() {
        let tmp = tempfile::tempdir().unwrap();
        let from = fixture(&tmp.path().join("et"));
        let dest = tmp.path().join("copy");
        fs::create_dir_all(&dest).unwrap();
        let dest = fs::canonicalize(&dest).unwrap();
        let plan = Plan::new(&from, &dest, "copy", false).unwrap();

        let err = copy_until(&plan, &AtomicBool::new(true), &|| ()).err().expect("interrupted");
        assert!(format!("{err:#}").starts_with("Interrupted; nothing was registered"), "{err:#}");
        assert_eq!(names(&dest), [".cactup-cp-copy.partial"]);

        copy_until(&plan, &AtomicBool::new(false), &|| ()).unwrap();
        assert_eq!(names(&dest), [".cactup", "My", "imports", "installation-source.th"]);
    }
}
