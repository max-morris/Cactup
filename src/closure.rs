//! The closure check (spec §7.5, §8.9): a thornlist must build every thorn
//! together with what it cannot compile or link without. `cactup build`
//! runs it before `make` (or the queue) and `sim export`/`sim pack` before
//! writing a `.sim`, so a list missing a thorn fails in seconds, naming the
//! thorn, rather than after a queue wait, and never leaves this machine as a
//! `.sim` (the Cactarium refuses one too).
//!
//! What a thorn needs is the shared crate's rule (`ccl::needs_of`), the one
//! the Cactarium's checks and `--trim` read: the implementations it inherits
//! or befriends, the capabilities, thorns and functions it requires, and the
//! `USES INCLUDE` headers its sources include outside any conditional. A
//! header counts only where a thorn cactup knows of provides it
//! (`ccl::counts`): here, the thorns of every repo the installation has
//! fetched, whole, not only the ones the list links into `arrangements/`.
//! That is how EinsteinInitialData/TwoPuncturesX without TwoPunctures is
//! caught: TwoPunctures is in the fetched repo, provides `TP_Guts.h`, and the
//! list does not build it.
//!
//! Sources are read only where they can matter: a thorn's, when one of its
//! `USES INCLUDE` headers is provided by a known thorn the list does not
//! build. A list that builds what it needs reads no source file at all.

use crate::thornlist::Thornlist;
use crate::Res;
use anyhow::bail;
use cactup_ecosystem_common::ccl::{self, Need, ThornFacts};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// One need a built thorn has and the list does not meet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unmet {
    /// The thorn's checkout, `Arrangement/Thorn`.
    pub thorn: String,
    pub need: Need,
    /// Known thorns that would meet it, as `Thorn (repo <name>)`.
    pub met_by: Vec<String>,
}

/// A thorn found in a fetched repo: its name, the repo it is in, its
/// directory, and its facts from the CCL alone.
struct Known {
    name: String,
    repo: String,
    facts: ThornFacts,
}

/// The needs the thorns `list` builds (from the tree under `cactus_root`)
/// leave unmet, among those that count. Empty when the list is closed, and
/// when a built thorn's `interface.ccl` cannot be read: Cactus says what is
/// wrong with that one itself, and nothing about what it might provide is
/// guessed here. Fails only when interrupted.
pub fn unmet(list: &Thornlist, cactus_root: &Path) -> Res<Vec<Unmet>> {
    let built = crate::sim::trim::thorn_facts(list, cactus_root)?;
    if built.values().any(|(_, facts)| facts.is_none()) {
        return Ok(Vec::new());
    }
    let mut built: Vec<(String, String, ThornFacts)> = built
        .into_iter()
        .map(|(checkout, (_, facts))| {
            let name = checkout.rsplit('/').next().unwrap_or(&checkout).to_owned();
            let mut facts = facts.expect("unreadable facts returned early");
            // As `--trim` reads them: a thorn without IMPLEMENTS implements
            // its own name. Cactus refuses to build such a thorn, and a
            // wrong guess here must never refuse a list.
            facts.implements.get_or_insert_with(|| name.clone());
            (checkout, name, facts)
        })
        .collect();
    let known = known_thorns(&cactus_root.join("repos"))?;

    // The headers worth reading sources for: a known thorn provides them,
    // and nothing the list builds does.
    let provides = |facts: &ThornFacts, h: &str| facts.provides_includes.iter().any(|p| p.eq_ignore_ascii_case(h));
    let wanted = |h: &str| {
        known.iter().any(|k| provides(&k.facts, h)) && !built.iter().any(|(_, _, f)| provides(f, h))
    };
    let candidates: Vec<usize> =
        (0..built.len()).filter(|&i| built[i].2.uses_includes.iter().any(|h| wanted(h))).collect();
    if !candidates.is_empty() {
        let dirs: Vec<(usize, PathBuf)> =
            candidates.iter().map(|&i| (i, cactus_root.join("arrangements").join(&built[i].0))).collect();
        let scanned = crate::par::parallel_map(&dirs, |(i, dir)| {
            let mut facts = built[*i].2.clone();
            // Stops at Ctrl-C; `parallel_map` then fails, so a cut-short scan is never read.
            facts.scan_sources_until(dir, &gix::interrupt::IS_INTERRUPTED);
            facts.needs_includes
        })?;
        for ((i, _), needs) in dirs.iter().zip(scanned) {
            built[*i].2.needs_includes = needs;
        }
    }

    let set: Vec<(&str, &ThornFacts)> = built.iter().map(|(_, name, f)| (name.as_str(), f)).collect();
    let everything: Vec<&ThornFacts> = known.iter().map(|k| &k.facts).chain(built.iter().map(|(_, _, f)| f)).collect();
    let mut out = Vec::new();
    for (checkout, _, facts) in &built {
        for need in ccl::unmet_among(facts, set.iter().copied(), everything.iter().copied()) {
            let mut met_by: Vec<String> = known
                .iter()
                .filter(|k| ccl::meets(&k.name, &k.facts, &need))
                .map(|k| format!("{} (repo {})", k.name, k.repo))
                .collect();
            met_by.dedup();
            out.push(Unmet { thorn: checkout.clone(), need, met_by });
        }
    }
    Ok(out)
}

/// Refuse to go on when [`unmet`] finds anything: `what` is the refusal's
/// start ("Not building config qc0"), and the rest names each thorn, what it
/// needs, and what in the fetched repos would meet it.
pub fn refuse_unmet(list: &Thornlist, cactus_root: &Path, what: &str) -> Res<()> {
    let unmet = unmet(list, cactus_root)?;
    if unmet.is_empty() {
        return Ok(());
    }
    let lines: Vec<String> = unmet
        .iter()
        .map(|u| match u.met_by.as_slice() {
            [] => format!("{} {}, and nothing fetched meets that", u.thorn, u.need),
            by => format!("{} {}; {} would meet it", u.thorn, u.need, by.join(", ")),
        })
        .collect();
    bail!(
        "{what}: the thornlist builds thorns without what they need, so they would not compile:\n  {}\nAdd what \
         meets each to the thornlist (or stop building the thorn that needs it)",
        lines.join("\n  ")
    )
}

/// The thorns of every fetched repo under `repos` (a repo is a thorn, a
/// directory of thorns, or of arrangements of thorns: the depths
/// `ccl::scan_tree` reads), from their CCL alone. On the parallel pool with
/// progress, one repo per item: on a network filesystem ~80 repos of
/// directory listings add up. A repo that cannot be read adds nothing.
fn known_thorns(repos: &Path) -> Res<Vec<Known>> {
    let Ok(rd) = std::fs::read_dir(repos) else { return Ok(Vec::new()) };
    let mut names: Vec<String> = rd
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.'))
        .collect();
    names.sort();
    let (progress, renderer) = crate::manifest::setup_prodash_if_tty();
    let item = progress.add_child("read the fetched repos' .ccl files");
    item.init(Some(names.len()), Some(prodash::unit::label("repos")));
    let item = std::sync::Mutex::new(item);
    let read = crate::par::parallel_map(&names, |repo| {
        let current = item.lock().expect("closure progress poisoned").add_child(repo.clone());
        let found = thorns_in(&repos.join(repo), repo);
        drop(current);
        item.lock().expect("closure progress poisoned").inc();
        found
    });
    drop(item);
    if let Some(renderer) = renderer {
        renderer.shutdown_and_wait();
    }
    Ok(read?.into_iter().flatten().collect())
}

/// The thorns at `root` (the repo itself), one level down and two levels
/// down, never through a symlink or into a hidden directory.
fn thorns_in(root: &Path, repo: &str) -> Vec<Known> {
    fn subdirs(dir: &Path) -> Vec<PathBuf> {
        let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
        let mut v: Vec<PathBuf> = rd
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
            .map(|e| e.path())
            .collect();
        v.sort();
        v
    }
    let mut dirs = vec![root.to_owned()];
    for a in subdirs(root) {
        dirs.extend(subdirs(&a));
        dirs.push(a);
    }
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for dir in dirs {
        let Some(interface) = crate::sim::trim::read_ccl(&dir.join("interface.ccl")) else { continue };
        let configuration = crate::sim::trim::read_ccl(&dir.join("configuration.ccl"));
        let mut facts = ThornFacts::scan(&interface, configuration.as_deref());
        // A repo that is itself a thorn is checked out as `../$1-$2`
        // (`ExternalLibraries-GSL` is the thorn GSL): its name is the part
        // after the dash, else what it implements.
        let name = if dir == root {
            (repo.rsplit_once('-').map(|(_, t)| t.to_owned()))
                .or_else(|| facts.implements.clone())
                .unwrap_or_else(|| repo.to_owned())
        } else {
            dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| repo.to_owned())
        };
        if seen.insert(dir.clone()) {
            facts.implements.get_or_insert_with(|| name.clone());
            out.push(Known { name, repo: repo.to_owned(), facts });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A tree like a fetched installation's: `repos/<repo>/<Thorn>` checked
    /// out whole, `arrangements/<Arr>/<Thorn>` linking only what the list builds.
    fn tree(root: &Path) {
        let thorn = |repo: &str, name: &str, iface: &str, conf: Option<&str>, src: &[(&str, &str)]| {
            let dir = root.join("repos").join(repo).join(name);
            fs::create_dir_all(dir.join("src")).unwrap();
            fs::write(dir.join("interface.ccl"), iface).unwrap();
            if let Some(conf) = conf {
                fs::write(dir.join("configuration.ccl"), conf).unwrap();
            }
            for (file, text) in src {
                fs::write(dir.join("src").join(file), text).unwrap();
            }
        };
        thorn(
            "einsteininitialdata",
            "TwoPuncturesX",
            "IMPLEMENTS: TwoPuncturesX\nINHERITS: ADMBaseX\nUSES INCLUDE: TP_Guts.h\nUSES INCLUDE: nompi.h\n",
            Some("REQUIRES GSL\n"),
            &[("TwoPunctures.cc", "#include \"TP_Guts.h\"\n#ifndef CCTK_MPI\n#include \"nompi.h\"\n#endif\n")],
        );
        thorn(
            "einsteininitialdata",
            "TwoPunctures",
            "IMPLEMENTS: TwoPunctures\nINCLUDE HEADER: TP_Guts.h in TP_Guts.h\n",
            None,
            &[],
        );
        thorn("utils", "NoMPI", "IMPLEMENTS: NoMPI\nINCLUDE HEADER: nompi.h in nompi.h\n", None, &[]);
        thorn("carpetx", "ADMBaseX", "IMPLEMENTS: ADMBaseX\n", None, &[]);
        let gsl = root.join("repos/ExternalLibraries-GSL");
        fs::create_dir_all(&gsl).unwrap();
        fs::write(gsl.join("interface.ccl"), "IMPLEMENTS: GSL\n").unwrap();
        fs::write(gsl.join("configuration.ccl"), "PROVIDES GSL\n{\n}\n").unwrap();
        for (arr, name, repo) in [
            ("EinsteinInitialData", "TwoPuncturesX", "repos/einsteininitialdata/TwoPuncturesX"),
            ("EinsteinInitialData", "TwoPunctures", "repos/einsteininitialdata/TwoPunctures"),
            ("CarpetX", "ADMBaseX", "repos/carpetx/ADMBaseX"),
            ("ExternalLibraries", "GSL", "repos/ExternalLibraries-GSL"),
        ] {
            fs::create_dir_all(root.join("arrangements").join(arr)).unwrap();
            std::os::unix::fs::symlink(root.join(repo), root.join("arrangements").join(arr).join(name)).unwrap();
        }
    }

    fn list(thorns: &[&str]) -> Thornlist {
        let mut text = String::from("!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n");
        for (i, t) in thorns.iter().enumerate() {
            text.push_str(&format!(
                "!TARGET = $ROOT/arrangements\n!TYPE = git\n!URL = https://x/{i}.git\n!REPO_PATH = $2\n!CHECKOUT =\n{t}\n\n"
            ));
        }
        crate::thornlist::parse(&text).unwrap()
    }

    #[test]
    fn a_thorn_built_without_the_one_whose_header_it_includes_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        tree(tmp.path());
        let open = list(&["EinsteinInitialData/TwoPuncturesX", "CarpetX/ADMBaseX", "ExternalLibraries/GSL"]);
        let found = unmet(&open, tmp.path()).unwrap();
        assert_eq!(
            found,
            [Unmet {
                thorn: "EinsteinInitialData/TwoPuncturesX".into(),
                need: Need::Include("TP_Guts.h".into()),
                met_by: vec!["TwoPunctures (repo einsteininitialdata)".into()],
            }],
            "nompi.h is included only under a conditional, so NoMPI is not needed"
        );
        let msg = format!("{:#}", refuse_unmet(&open, tmp.path(), "Not building config qc0").unwrap_err());
        assert!(msg.starts_with("Not building config qc0: "), "{msg}");
        assert!(msg.contains("EinsteinInitialData/TwoPuncturesX includes the header TP_Guts.h; TwoPunctures (repo einsteininitialdata) would meet it"), "{msg}");
        assert!(!msg.contains('§'), "no spec refs in user-visible text");

        let closed = list(&[
            "EinsteinInitialData/TwoPuncturesX",
            "EinsteinInitialData/TwoPunctures",
            "CarpetX/ADMBaseX",
            "ExternalLibraries/GSL",
        ]);
        assert!(unmet(&closed, tmp.path()).unwrap().is_empty());
        refuse_unmet(&closed, tmp.path(), "x").unwrap();
    }

    #[test]
    fn ccl_needs_are_checked_too() {
        let tmp = tempfile::tempdir().unwrap();
        tree(tmp.path());
        let found = unmet(&list(&["EinsteinInitialData/TwoPuncturesX", "EinsteinInitialData/TwoPunctures"]), tmp.path()).unwrap();
        let needs: Vec<String> = found.iter().map(|u| u.need.to_string()).collect();
        assert_eq!(needs, ["inherits or befriends ADMBaseX", "requires the capability GSL"]);
        assert_eq!(found[1].met_by, ["GSL (repo ExternalLibraries-GSL)"]);
    }

    /// A header nothing fetched provides is a hook: it compiles empty.
    #[test]
    fn a_header_nothing_provides_is_no_need() {
        let tmp = tempfile::tempdir().unwrap();
        tree(tmp.path());
        fs::remove_dir_all(tmp.path().join("repos/einsteininitialdata/TwoPunctures")).unwrap();
        let open = list(&["EinsteinInitialData/TwoPuncturesX", "CarpetX/ADMBaseX", "ExternalLibraries/GSL"]);
        assert!(unmet(&open, tmp.path()).unwrap().is_empty());
    }
}
