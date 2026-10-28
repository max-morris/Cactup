//! Test selection (§11.3): `test run`/`test submit` take test names, thorns
//! (`Thorn` or `arrangement/Thorn`), `Thorn/test` pairs and arrangements.
//! The flesh harness knows only `Thorn` and `Thorn/test`
//! (CCTK_TESTSUITE_RUN_TESTS, matched in RunTestUtils.pl's
//! FindTestParameterFiles), so an `arrangement/Thorn` handed over verbatim
//! reads as thorn `arrangement`, test `Thorn`, and silently selects nothing.
//! [`resolve`] translates against the config's ThornList at submit time; the
//! result is frozen into @TESTSUITE_SELECT@, so the compute node never
//! re-resolves (D11).

use crate::Res;
use anyhow::{bail, Context};
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

/// One ThornList entry and the tests the flesh would find for it.
struct Thorn {
    arrangement: String,
    name: String,
    tests: BTreeSet<String>,
}

/// The selection in the flesh's CCTK_TESTSUITE_RUN_TESTS format: empty runs
/// everything, otherwise space-separated `Thorn` / `Thorn/test` entries.
/// Any selector that names nothing with tests in the config fails the whole
/// selection, rather than queueing a job that runs zero tests.
pub(crate) fn resolve(cactus_root: &Path, cfg_name: &str, selectors: &[String]) -> Res<String> {
    let selectors: Vec<&str> = selectors.iter().flat_map(|s| s.split_whitespace()).collect();
    if selectors.is_empty() {
        return Ok(String::new());
    }
    let thorns = scan_thorns(cactus_root, cfg_name)?;
    let mut entries: Vec<String> = Vec::new();
    let mut problems = Vec::new();
    for sel in selectors {
        match resolve_one(sel, &thorns) {
            Ok(found) => {
                for entry in found {
                    if !entries.contains(&entry) {
                        entries.push(entry);
                    }
                }
            }
            Err(why) => problems.push(why),
        }
    }
    if !problems.is_empty() {
        bail!(
            "Test selection for config \"{cfg_name}\" matches nothing:\n  {}\n\
             A selector is a test, a thorn (Thorn or arrangement/Thorn), Thorn/test, or an \
             arrangement in 'configs/{cfg_name}/ThornList'.",
            problems.join("\n  ")
        );
    }
    Ok(entries.join(" "))
}

/// A bare name is an arrangement, else a thorn, else a test name; the
/// arrangement wins so that every reading has a spelling (`CarpetX` is the
/// arrangement, `CarpetX/CarpetX` the thorn). With a slash it is
/// `arrangement/Thorn`, else `Thorn/test`.
fn resolve_one(sel: &str, thorns: &[Thorn]) -> Result<Vec<String>, String> {
    match sel.split('/').collect::<Vec<_>>()[..] {
        [name] if !name.is_empty() => {
            let in_arr: Vec<&Thorn> = thorns.iter().filter(|t| t.arrangement == name).collect();
            if !in_arr.is_empty() {
                let tested: Vec<String> =
                    in_arr.iter().filter(|t| !t.tests.is_empty()).map(|t| t.name.clone()).collect();
                return if tested.is_empty() {
                    Err(format!("{sel}: arrangement {name} has no tests"))
                } else {
                    Ok(tested)
                };
            }
            if let Some(thorn) = thorns.iter().find(|t| t.name == name) {
                return if thorn.tests.is_empty() {
                    Err(format!("{sel}: thorn {}/{name} has no tests", thorn.arrangement))
                } else {
                    Ok(vec![name.to_owned()])
                };
            }
            let with_test: Vec<String> = thorns
                .iter()
                .filter(|t| t.tests.contains(name))
                .map(|t| format!("{}/{name}", t.name))
                .collect();
            if with_test.is_empty() {
                Err(format!("{sel}: no arrangement, thorn or test of that name"))
            } else {
                Ok(with_test)
            }
        }
        [first, second] if !first.is_empty() && !second.is_empty() => {
            if let Some(thorn) = thorns.iter().find(|t| t.arrangement == first && t.name == second) {
                return if thorn.tests.is_empty() {
                    Err(format!("{sel}: thorn has no tests"))
                } else {
                    Ok(vec![second.to_owned()])
                };
            }
            match thorns.iter().find(|t| t.name == first) {
                Some(thorn) if thorn.tests.contains(second) => Ok(vec![sel.to_owned()]),
                Some(thorn) => {
                    Err(format!("{sel}: thorn {}/{first} has no test {second}", thorn.arrangement))
                }
                None => Err(format!(
                    "{sel}: no thorn {first}/{second}, and no thorn {first} with test {second}"
                )),
            }
        }
        _ => Err(format!("{sel}: not a test, thorn or arrangement name")),
    }
}

/// The config's ThornList, read the way the flesh harness reads it (lines
/// starting `#` or `!` skipped, the first word taken as `arrangement/Thorn`,
/// duplicates dropped), with each thorn's tests listed on a fixed-width pool:
/// one `test/` directory listing per thorn, latency-bound on NFS, so ~400 of
/// them get a phase-scoped bar (§2.4). Nothing here calls `info`/`fail`, so
/// the tty-only renderer loses nothing on a piped stderr.
fn scan_thorns(cactus_root: &Path, cfg_name: &str) -> Res<Vec<Thorn>> {
    let path = cactus_root.join("configs").join(cfg_name).join("ThornList");
    let text =
        fs::read_to_string(&path).with_context(|| format!("Failed to read '{}'", path.display()))?;
    let mut seen = BTreeSet::new();
    let mut listed: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        let line = line.trim_start();
        if line.starts_with('#') || line.starts_with('!') {
            continue;
        }
        let Some(word) = line.split_whitespace().next() else { continue };
        let Some((arrangement, name)) = word.split_once('/') else { continue };
        if seen.insert(word.to_owned()) {
            listed.push((arrangement.to_owned(), name.to_owned()));
        }
    }
    let arrangements = cactus_root.join("arrangements");
    let (progress, renderer) = crate::manifest::setup_prodash_if_tty();
    let scanning = progress.add_child("find tests");
    scanning.init(Some(listed.len()), Some(prodash::unit::label("thorns")));
    let scanning = std::sync::Mutex::new(scanning);
    let tests = crate::par::parallel_map(&listed, |(arrangement, name)| {
        let current = scanning.lock().expect("test-scan progress poisoned").add_child(name.clone());
        let tests = thorn_tests(&arrangements.join(arrangement).join(name).join("test"));
        drop(current);
        scanning.lock().expect("test-scan progress poisoned").inc();
        tests
    });
    drop(scanning);
    if let Some(renderer) = renderer {
        renderer.shutdown_and_wait();
    }
    let tests = tests?;
    Ok(listed
        .into_iter()
        .zip(tests)
        .map(|((arrangement, name), tests)| Thorn { arrangement, name, tests })
        .collect())
}

/// The flesh's notion of a thorn's tests: every `<name>.par` in `test/` with
/// a `<name>` directory or a `<name>.tar[.gz|.bz2|.xz]`/`.tgz`/`.tbz`/`.txz`
/// beside it. A missing or unreadable directory has none.
fn thorn_tests(test_dir: &Path) -> BTreeSet<String> {
    let Ok(listing) = fs::read_dir(test_dir) else { return BTreeSet::new() };
    let mut dirs = BTreeSet::new();
    let mut files = BTreeSet::new();
    for entry in listing.flatten() {
        let Ok(name) = entry.file_name().into_string() else { continue };
        // `metadata` follows symlinks, as the flesh's `-d`/`-f` do.
        match entry.metadata() {
            Ok(m) if m.is_dir() => dirs.insert(name),
            Ok(_) => files.insert(name),
            Err(_) => false,
        };
    }
    const ARCHIVES: [&str; 7] = [".tar", ".tar.gz", ".tgz", ".tar.bz2", ".tbz", ".tar.xz", ".txz"];
    files
        .iter()
        .filter_map(|f| f.strip_suffix(".par"))
        .filter(|t| dirs.contains(*t) || ARCHIVES.iter().any(|ext| files.contains(&format!("{t}{ext}"))))
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tree whose ThornList has CarpetX/{CarpetX,BoxInBox,NoTests} and
    /// McLachlan/ML_BSSN; BoxInBox's second test ships as a tarball, and an
    /// orphan .par without output does not count (as in the flesh).
    fn tree() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("configs/c")).unwrap();
        fs::write(
            root.join("configs/c/ThornList"),
            "# comment\nCarpetX/CarpetX\nCarpetX/BoxInBox   # trailing\n!ignored/Thorn\n\
             CarpetX/NoTests\nMcLachlan/ML_BSSN\nCarpetX/BoxInBox\n",
        )
        .unwrap();
        let add = |thorn: &str, test: &str, tarball: bool| {
            let dir = root.join("arrangements").join(thorn).join("test");
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(format!("{test}.par")), "").unwrap();
            if tarball {
                fs::write(dir.join(format!("{test}.tar.gz")), "").unwrap();
            } else {
                fs::create_dir_all(dir.join(test)).unwrap();
            }
        };
        add("CarpetX/CarpetX", "test_grid", false);
        add("CarpetX/BoxInBox", "boxinbox", false);
        add("CarpetX/BoxInBox", "shared_name", true);
        add("McLachlan/ML_BSSN", "ML_BSSN_sgw3d", false);
        add("McLachlan/ML_BSSN", "shared_name", false);
        fs::write(root.join("arrangements/McLachlan/ML_BSSN/test/orphan.par"), "").unwrap();
        fs::create_dir_all(root.join("arrangements/CarpetX/NoTests")).unwrap();
        tmp
    }

    fn sel(tmp: &tempfile::TempDir, selectors: &[&str]) -> Res<String> {
        let owned: Vec<String> = selectors.iter().map(|s| s.to_string()).collect();
        resolve(tmp.path(), "c", &owned)
    }

    #[test]
    fn every_documented_form_reaches_the_flesh_format() {
        let tmp = tree();
        assert_eq!(sel(&tmp, &[]).unwrap(), "", "empty runs everything");
        // The reported bug: arrangement/Thorn must not reach the flesh verbatim.
        assert_eq!(sel(&tmp, &["CarpetX/BoxInBox"]).unwrap(), "BoxInBox");
        assert_eq!(sel(&tmp, &["BoxInBox"]).unwrap(), "BoxInBox");
        assert_eq!(sel(&tmp, &["McLachlan"]).unwrap(), "ML_BSSN");
        // An arrangement selects only thorns that have tests.
        assert_eq!(sel(&tmp, &["CarpetX"]).unwrap(), "CarpetX BoxInBox");
        assert_eq!(sel(&tmp, &["CarpetX/CarpetX"]).unwrap(), "CarpetX");
        assert_eq!(sel(&tmp, &["ML_BSSN/ML_BSSN_sgw3d"]).unwrap(), "ML_BSSN/ML_BSSN_sgw3d");
        assert_eq!(sel(&tmp, &["boxinbox"]).unwrap(), "BoxInBox/boxinbox");
        assert_eq!(sel(&tmp, &["shared_name"]).unwrap(), "BoxInBox/shared_name ML_BSSN/shared_name");
        // Several selectors, also within one argument; duplicates collapse.
        assert_eq!(sel(&tmp, &["BoxInBox McLachlan/ML_BSSN", "BoxInBox"]).unwrap(), "BoxInBox ML_BSSN");
    }

    #[test]
    fn a_selector_naming_nothing_fails_the_selection() {
        let tmp = tree();
        for (bad, why) in [
            ("Nope", "Nope: no arrangement, thorn or test of that name"),
            ("orphan", "orphan: no arrangement, thorn or test of that name"),
            ("NoTests", "NoTests: thorn CarpetX/NoTests has no tests"),
            ("CarpetX/NoTests", "CarpetX/NoTests: thorn has no tests"),
            ("ML_BSSN/nope", "ML_BSSN/nope: thorn McLachlan/ML_BSSN has no test nope"),
            ("Foo/Bar", "Foo/Bar: no thorn Foo/Bar, and no thorn Foo with test Bar"),
            ("ignored/Thorn", "no thorn ignored/Thorn"),
            ("a/b/c", "a/b/c: not a test, thorn or arrangement name"),
        ] {
            let err = sel(&tmp, &["BoxInBox", bad]).unwrap_err().to_string();
            assert!(err.contains(why), "{bad}: {err}");
            assert!(!err.contains("§"), "user-visible: {err}");
        }
    }
}
