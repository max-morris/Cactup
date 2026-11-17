//! `cactup add` and `cactup remove` resolving dependencies, end to end: real
//! upstream repos on disk, an installation fetched from them, and a fake
//! Cactarium (`test_server`) serving versions, `/resolve`, `/lookup` and
//! `/providers` in the crate's shapes.
// Plan §8 (integration tests), the P3 row of §7's reviewer checks.

use super::*;
use crate::installation::{AddedFor, InstallationMeta};
use cactup_ecosystem_common::ccl::ThornFacts;
use std::sync::{Arc, Mutex};

/// Every request the fake Cactarium got: its path and body.
type Log = Arc<Mutex<Vec<(String, String)>>>;

/// A thorn of a version: `Arrangement`, `Thorn`, and its facts.
fn thorn(arrangement: &str, name: &str, facts: &ThornFacts) -> String {
    let facts = serde_json::to_string(facts).unwrap();
    format!(r#"{{"arrangement":"{arrangement}","thorn":"{name}","repo-path":"$2","facts":{facts}}}"#)
}

/// One `PackageVersion`, its thorns in arrangement `arrangement`.
fn version(v: &str, commit: &str, url: &str, arrangement: &str, thorns: &[&str], claims: &str) -> String {
    let thorns: Vec<String> = thorns.iter().map(|t| thorn(arrangement, t, &ThornFacts::default())).collect();
    version_with(v, commit, url, &thorns, claims, false)
}

fn version_with(v: &str, commit: &str, url: &str, thorns: &[String], claims: &str, flesh: bool) -> String {
    format!(
        r#"{{"version":"{v}","track":null,"tag":null,"commit":"{commit}","tree":null,"yanked":false,
        "yank-reason":null,"state":"ok","curated":false,"published":0,"url":"{url}","flesh":{flesh},
        "thorns":[{}],"claims":{claims}}}"#,
        thorns.join(",")
    )
}

/// A dependency claim of `target` at `range`, needing `needs`.
fn depends(target: &str, range: &str, needs: &[&str]) -> String {
    let needs: Vec<String> = needs.iter().map(|n| format!("\"{n}\"")).collect();
    format!(r#"{{"kind":"dependency","target":"{target}","range":"{range}","needs":[{}],"note":null}}"#, needs.join(","))
}

/// The fake Cactarium: `packages` (id → its versions, newest first), the
/// pins `/lookup` knows (URL → package and version), and what `/providers`
/// answers with.
struct Fake {
    packages: Vec<(String, Vec<String>)>,
    pins: Vec<(String, String, Option<String>)>,
    providers: String,
}

impl Fake {
    fn new() -> Fake {
        Fake { packages: Vec::new(), pins: Vec::new(), providers: r#"{"providers":[]}"#.to_owned() }
    }

    fn package(mut self, id: &str, versions: Vec<String>) -> Fake {
        self.packages.push((id.to_owned(), versions));
        self
    }

    fn pin(mut self, url: &str, package: &str, version: Option<&str>) -> Fake {
        self.pins.push((url.to_owned(), package.to_owned(), version.map(str::to_owned)));
        self
    }

    fn serve(self) -> (String, Log) {
        let log: Log = Arc::default();
        let seen = log.clone();
        let (base, _) = crate::fetch::download::test_server::serve_with_body(1000, move |path, body| {
            seen.lock().unwrap().push((path.to_owned(), String::from_utf8_lossy(body).into_owned()));
            let ok = |body: String| ("200 OK", "api-generation: 1\r\nx-cactarium-untrusted: false\r\n".to_owned(), body.into_bytes());
            let missing = || {
                let body = r#"{"error":{"code":"not-found","message":"No such package"}}"#;
                ("404 Not Found", "api-generation: 1\r\n".to_owned(), body.as_bytes().to_vec())
            };
            let path = path.strip_prefix("/api/v1").unwrap_or(path);
            if let Some(req) = path.strip_prefix("/resolve?req=") {
                let req = req.split('&').next().unwrap().replace("%40", "@").replace("%23", "#");
                let id = req.split(['@', '#']).next().unwrap();
                return match self.packages.iter().find(|(p, _)| p == id) {
                    Some(_) => ok(format!(
                        r#"{{"slug":"{id}","kind":"package","url":"u","contact":null,"version":"1.0.0","track":null,
                        "tag":null,"commit":null,"tree":null,"yanked":false,"yank-reason":null,"state":"ok",
                        "curated":false,"published":0}}"#
                    )),
                    None => missing(),
                };
            }
            if path == "/lookup" {
                let asked: cactup_ecosystem_common::api::LookupRequest = serde_json::from_slice(body).unwrap();
                let pins: Vec<String> = (asked.pins.iter())
                    .map(|p| {
                        let known = self.pins.iter().find(|(url, _, _)| *url == p.url);
                        let package = known.map_or("null".to_owned(), |(_, pkg, _)| format!("\"{pkg}\""));
                        let version = known
                            .and_then(|(_, _, v)| v.as_ref())
                            .map_or("null".to_owned(), |v| format!("\"{v}\""));
                        // The real server often cannot say whether its mirror
                        // has the commit: null.
                        format!(
                            r#"{{"url":"{}","commit":"{}","package":{package},"version":{version},"flesh":false,
                            "fork-of":null,"commit-known":null}}"#,
                            p.url, p.commit
                        )
                    })
                    .collect();
                return ok(format!(r#"{{"pins":[{}]}}"#, pins.join(",")));
            }
            if path.starts_with("/providers?") {
                return ok(self.providers.clone());
            }
            if let Some(rest) = path.strip_prefix("/packages/") {
                let (id, at) = match rest.split_once("/versions") {
                    Some((id, at)) => (id, at.trim_start_matches('/')),
                    None => return missing(),
                };
                let Some((_, versions)) = self.packages.iter().find(|(p, _)| p == id) else { return missing() };
                if at.is_empty() {
                    return ok(format!(r#"{{"slug":"{id}","versions":[{}]}}"#, versions.join(",")));
                }
                return match versions.iter().find(|v| v.contains(&format!("\"commit\":\"{at}"))) {
                    Some(v) => ok(v.clone()),
                    None => missing(),
                };
            }
            missing()
        });
        (base, log)
    }
}

/// An upstream repo `name` with `files`, as a local URL and its commit.
fn upstream(tmp: &std::path::Path, name: &str, files: &[(&str, &str)]) -> (String, String) {
    let dir = tmp.join("up").join(name);
    if !dir.exists() {
        crate::fetch::git::testrepo::init(&dir);
    }
    (dir.display().to_string(), crate::fetch::tests::commit_files(&dir, files).to_string())
}

/// A base thornlist section: `url` at `commit`, checking out `checkouts`.
fn section(url: &str, commit: &str, checkouts: &[&str]) -> String {
    format!(
        "\n!TARGET = $ROOT/arrangements\n!TYPE = git\n!URL = {url}\n!REPO_PATH = $2\n!REPO_COMMIT = {commit}\n\
         !CHECKOUT =\n{}\n",
        checkouts.join("\n")
    )
}

/// An installation (alias `t`, active) of `base`, fetched, with the
/// Cactarium at `cactarium`.
struct Setup {
    _tmp: tempfile::TempDir,
    ctx: Ctx,
    inst: Installation,
}

impl Setup {
    fn new(tmp: tempfile::TempDir, base: &str, cactarium: &str) -> Setup {
        let root = tmp.path().join("inst");
        let db = crate::database::Db::in_dir(&tmp.path().join("db"));
        let path = root.display().to_string();
        let cactarium = cactarium.to_owned();
        db.update(move |d| {
            d.installations.insert(
                "t".into(),
                crate::database::CactusInstallation {
                    alias: "t".into(),
                    release: None,
                    path,
                    thornlist: None,
                    simfile: None,
                    stack: None,
                    current_release: None,
                    current_thornlist: None,
                    stack_contact: None,
                    unfetched_repos: Default::default(),
                },
            );
            d.active_installation = Some("t".into());
            d.set_knob(crate::cactarium::URL_KNOB, cactarium);
            Ok(())
        })
        .unwrap();
        let ctx = Ctx {
            globals: crate::args::GlobalOpts {
                verbose: false,
                trace: false,
                manifest_url: String::new(),
                mdb_path: None,
                machine: None,
                installation: None,
                hostname: None,
                knob: Vec::new(),
                risky_allow_untrusted_artifacts: false,
            },
            db,
        };
        let inst = Installation::resolve(&ctx).unwrap();
        let pristine = format!("!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n{base}");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(inst.source_thornlist(), &pristine).unwrap();
        let list = thornlist::parse(&pristine).unwrap();
        let mut progress = prodash::tree::Root::new().add_child("plan");
        let plan = crate::fetch::plan(&list, &inst.root, &mut progress).unwrap();
        let report = crate::fetch::execute(&plan, &inst.root).unwrap();
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        crate::fetch::FetchState::record(&inst.root, &report.records()).unwrap();
        let live = inst.live_thornlist().unwrap();
        std::fs::create_dir_all(live.parent().unwrap()).unwrap();
        std::fs::write(&live, &pristine).unwrap();
        Setup { _tmp: tmp, ctx, inst }
    }

    fn meta(&self) -> InstallationMeta {
        self.inst.meta().unwrap()
    }

    fn added(&self) -> Vec<(String, Vec<String>)> {
        (self.meta().added.iter())
            .map(|a| (a.package.clone(), a.thorns.iter().map(|t| t.checkout.clone()).collect()))
            .collect()
    }

    fn add(&self, specs: &[&str], flags: AddFlags) -> Res<()> {
        let specs: Vec<String> = specs.iter().map(|s| (*s).to_owned()).collect();
        dispatch(&self.ctx, &specs, &flags)
    }

    fn remove(&self, packages: &[&str], flags: RemoveFlags) -> Res<()> {
        let packages: Vec<String> = packages.iter().map(|s| (*s).to_owned()).collect();
        remove(&self.ctx, &packages, &flags)
    }

    /// Rewrites the record by hand (as an older binary, or a stale run,
    /// would leave it), keeping whatever `set_meta` would drop.
    fn write_meta(&self, meta: &InstallationMeta) {
        let path = self.inst.cactup_dir().join("installation.toml");
        crate::format::write(&path, meta, &crate::installation::INSTALLATION).unwrap();
    }
}

fn yes() -> AddFlags {
    AddFlags { yes: true, ..AddFlags::default() }
}

fn remove_yes() -> RemoveFlags {
    RemoveFlags { yes: true, ..RemoveFlags::default() }
}

/// max/z4c 1.0.0 (thorn Z4c) depends on et/carpetx ^1.0 for its CarpetX and
/// Loop (et/carpetx has Arith too); the installation's base is empty.
struct Z4c {
    setup: Setup,
    z: String,
    c: String,
    log: Log,
}

fn z4c() -> Z4c {
    let tmp = tempfile::tempdir().unwrap();
    let (zurl, z) = upstream(tmp.path(), "z4c", &[("Z4c/interface.ccl", "IMPLEMENTS: z4c\n")]);
    let (curl, c) = upstream(
        tmp.path(),
        "carpetx",
        &[
            ("CarpetX/interface.ccl", "IMPLEMENTS: carpetx\n"),
            ("Loop/interface.ccl", "IMPLEMENTS: loop\n"),
            ("Arith/interface.ccl", "IMPLEMENTS: arith\n"),
        ],
    );
    let claims = format!("[{}]", depends("et/carpetx", "^1.0", &["CarpetX", "Loop"]));
    let (base, log) = Fake::new()
        .package("max/z4c", vec![version("1.0.0", &z, &zurl, "Z4c", &["Z4c"], &claims)])
        .package("et/carpetx", vec![version("1.0.0", &c, &curl, "CarpetX", &["CarpetX", "Loop", "Arith"], "[]")])
        .serve();
    Z4c { setup: Setup::new(tmp, "", &base), z, c, log }
}

/// `cactup add max/z4c`: the plan brings in et/carpetx's CarpetX and Loop,
/// so it is shown and confirmed; without a terminal it is refused unless
/// `--yes`, `--dry-run` changes nothing, and with `--yes` both are added,
/// carpetx's thorns into carpetx's own clone, each with why it is there.
#[test]
fn add_brings_in_the_needs_of_a_dependency() {
    let it = z4c();
    let s = &it.setup;
    let err = format!("{:#}", s.add(&["max/z4c"], AddFlags::default()).unwrap_err());
    assert!(err.contains("pass --yes, or --dry-run to see it"), "{err}");
    assert!(s.meta().added.is_empty());
    s.add(&["max/z4c"], AddFlags { dry_run: true, ..AddFlags::default() }).unwrap();
    assert!(s.meta().added.is_empty());

    s.add(&["max/z4c"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(
        s.added(),
        [
            ("max/z4c".to_owned(), vec!["Z4c/Z4c".to_owned()]),
            ("et/carpetx".to_owned(), vec!["CarpetX/CarpetX".to_owned(), "CarpetX/Loop".to_owned()]),
        ]
    );
    let meta = s.meta();
    let carpetx = meta.added.iter().find(|a| a.package == "et/carpetx").unwrap();
    assert_eq!((carpetx.repo.as_str(), carpetx.commit.as_str()), ("carpetx", it.c.as_str()));
    let arrangements = s.inst.root.join("Cactus/arrangements");
    assert_eq!(std::fs::read_to_string(arrangements.join("CarpetX/Loop/interface.ccl")).unwrap(), "IMPLEMENTS: loop\n");
    assert!(!arrangements.join("CarpetX/Arith").exists(), "only what the dependency needs");
    assert_eq!(
        meta.added_for["max/z4c"],
        AddedFor { requested: true, by: vec![], for_commit: Some(it.z.clone()), choices: vec![] }
    );
    assert_eq!(
        meta.added_for["et/carpetx"],
        AddedFor { requested: false, by: vec!["max/z4c@1.0.0".to_owned()], for_commit: Some(it.c.clone()), choices: vec![] }
    );
    // No pins in the base: nothing to look up.
    assert!(!it.log.lock().unwrap().iter().any(|(p, _)| p.contains("/lookup")));
}

/// `cactup remove`: a package another depends on is refused, naming the
/// dependent; removing the dependent takes along what only it needed, once
/// confirmed.
#[test]
fn remove_refuses_a_dependency_and_takes_orphans_along() {
    let it = z4c();
    let s = &it.setup;
    s.add(&["max/z4c"], yes()).unwrap();

    let err = format!("{:#}", s.remove(&["et/carpetx"], remove_yes()).unwrap_err());
    assert!(err.contains("Other packages depend on et/carpetx") && err.contains("max/z4c@1.0.0"), "{err}");
    assert!(err.contains("--no-deps"), "{err}");
    assert_eq!(s.meta().added.len(), 2);

    let err = format!("{:#}", s.remove(&["max/z4c"], RemoveFlags::default()).unwrap_err());
    assert!(err.contains("pass --yes, or --dry-run"), "{err}");
    s.remove(&["max/z4c"], RemoveFlags { dry_run: true, ..RemoveFlags::default() }).unwrap();
    assert_eq!(s.meta().added.len(), 2);

    s.remove(&["max/z4c"], remove_yes()).unwrap_or_else(|e| panic!("{e:#}"));
    let meta = s.meta();
    assert!(meta.added.is_empty() && meta.added_for.is_empty(), "{meta:?}");
}

/// A package whose `added-for` entry is missing (an installation from
/// before entries were kept) or stale (written for another commit: an older
/// binary re-added it) counts as named: no orphan pass takes it.
#[test]
fn a_missing_or_stale_added_for_never_orphans_a_package() {
    for stale in [false, true] {
        let it = z4c();
        let s = &it.setup;
        s.add(&["max/z4c"], yes()).unwrap();
        let mut meta = s.meta();
        if stale {
            meta.added_for.get_mut("et/carpetx").unwrap().for_commit = Some("f".repeat(40));
        } else {
            meta.added_for.remove("et/carpetx");
        }
        s.write_meta(&meta);
        s.remove(&["max/z4c"], remove_yes()).unwrap_or_else(|e| panic!("{e:#}"));
        assert_eq!(s.added(), [("et/carpetx".to_owned(), vec!["CarpetX/CarpetX".to_owned(), "CarpetX/Loop".to_owned()])]);
    }
}

/// Without the Cactarium, `remove` cannot tell what depends on what: it
/// refuses, naming `--no-deps`, which removes exactly what was named.
#[test]
fn remove_offline_refuses_unless_no_deps() {
    let it = z4c();
    let s = &it.setup;
    s.add(&["max/z4c"], yes()).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    s.ctx
        .db
        .update(move |d| {
            d.set_knob(crate::cactarium::URL_KNOB, format!("http://127.0.0.1:{port}"));
            Ok(())
        })
        .unwrap();
    let err = format!("{:#}", s.remove(&["max/z4c"], remove_yes()).unwrap_err());
    assert!(err.contains("isn't reachable") && err.contains("pass --no-deps"), "{err}");
    assert_eq!(s.meta().added.len(), 2);
    s.remove(&["max/z4c"], RemoveFlags { no_deps: true, ..RemoveFlags::default() }).unwrap();
    assert_eq!(s.added(), [("et/carpetx".to_owned(), vec!["CarpetX/CarpetX".to_owned(), "CarpetX/Loop".to_owned()])]);
}

/// A base of et/admbase (ADMBase, ADMCoupling) at 1.0.0 whose repo also has
/// StaticConformal, and 2.0.0 a commit later; max/app 1.0.0 needs
/// StaticConformal of et/admbase `range`.
struct Admbase {
    setup: Setup,
    a1: String,
    a2: String,
    log: Log,
}

fn admbase(range: &str) -> Admbase {
    let tmp = tempfile::tempdir().unwrap();
    let files = [
        ("ADMBase/interface.ccl", "IMPLEMENTS: admbase\n"),
        ("ADMCoupling/interface.ccl", "IMPLEMENTS: admcoupling\n"),
        ("StaticConformal/interface.ccl", "IMPLEMENTS: staticconformal\n"),
    ];
    let (aurl, a1) = upstream(tmp.path(), "admbase", &files);
    let (_, a2) = upstream(tmp.path(), "admbase", &[files[0], files[1], files[2], ("README", "2\n")]);
    let (purl, p) = upstream(tmp.path(), "app", &[("App/interface.ccl", "IMPLEMENTS: app\n")]);
    let thorns = ["ADMBase", "ADMCoupling", "StaticConformal"];
    let claims = format!("[{}]", depends("et/admbase", range, &["StaticConformal"]));
    let (base, log) = Fake::new()
        .package("max/app", vec![version("1.0.0", &p, &purl, "App", &["App"], &claims)])
        .package(
            "et/admbase",
            vec![
                version("2.0.0", &a2, &aurl, "ADMBase", &thorns, "[]"),
                version("1.0.0", &a1, &aurl, "ADMBase", &thorns, "[]"),
            ],
        )
        .pin(&aurl, "et/admbase", Some("1.0.0"))
        .serve();
    let setup = Setup::new(tmp, &section(&aurl, &a1, &["ADMBase/ADMBase", "ADMBase/ADMCoupling"]), &base);
    Admbase { setup, a1, a2, log }
}

/// The plan for `specs` on the setup's installation, and the records it
/// writes.
fn records(s: &Setup, specs: &[&str], flags: &AddFlags) -> Res<Vec<AddedPackage>> {
    let pristine = std::fs::read_to_string(s.inst.source_thornlist()).unwrap();
    let base = thornlist::parse(&pristine).unwrap();
    let meta = s.meta();
    let client = crate::cactarium::Client::new(&s.ctx.db.read().unwrap());
    let specs: Vec<String> = specs.iter().map(|s| (*s).to_owned()).collect();
    let (plan, present) = plan_add(&client, &s.inst.root, &base, &meta, None, &specs, flags)?;
    let records = records_of(&plan, &present, &meta)?;
    place(&base, &meta, records, &plan, &mut Vec::new())
}

/// The record of `package` among `records`: version, commit, thorns, replaces.
fn record_of(records: &[AddedPackage], package: &str) -> (String, String, Vec<String>, Vec<String>, String) {
    let r = records.iter().find(|r| r.package == package).unwrap_or_else(|| panic!("no {package} in {records:?}"));
    let thorns = r.thorns.iter().map(|t| t.checkout.clone()).collect();
    (r.version.clone(), r.commit.clone(), thorns, r.replaces.clone(), r.repo.clone())
}

/// A dependency that moves a base package: refused without `--upgrade`,
/// which names the flag; with it, the package moves carrying every thorn
/// the installation had enabled plus the new one, into the base's own
/// clone, in place of the base repo. `--upgrade-all`, naming the package
/// itself under `--upgrade`, and naming the version with `--replace` all
/// write that same record.
#[test]
fn upgrades_carry_the_present_enabled_set_and_the_flags_agree() {
    let it = admbase("^2.0");
    let s = &it.setup;
    let err = format!("{:#}", records(s, &["max/app"], &AddFlags::default()).unwrap_err());
    assert!(err.contains("et/admbase@1.0.0") && err.contains("Pass --upgrade"), "{err}");

    let want = (
        "2.0.0".to_owned(),
        it.a2.clone(),
        vec!["ADMBase/ADMBase".to_owned(), "ADMBase/ADMCoupling".to_owned(), "ADMBase/StaticConformal".to_owned()],
        vec!["admbase".to_owned()],
        "admbase".to_owned(),
    );
    let sorted = |mut r: (String, String, Vec<String>, Vec<String>, String)| {
        r.2.sort();
        r
    };
    let upgrade = AddFlags { upgrade: true, ..AddFlags::default() };
    let got = records(s, &["max/app"], &upgrade).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(sorted(record_of(&got, "et/admbase")), want);
    let all = AddFlags { upgrade_all: true, ..AddFlags::default() };
    let got = records(s, &["max/app"], &all).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(sorted(record_of(&got, "et/admbase")), want);
    let got = records(s, &["et/admbase"], &upgrade).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(sorted(record_of(&got, "et/admbase")), want);
    let replace = AddFlags { replace: true, ..AddFlags::default() };
    let got = records(s, &["et/admbase@2.0.0"], &replace).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(sorted(record_of(&got, "et/admbase")), want);

    // And on disk: the base's clone moves to 2.0.0 with all three thorns.
    s.add(&["max/app"], AddFlags { upgrade: true, yes: true, ..AddFlags::default() }).unwrap_or_else(|e| panic!("{e:#}"));
    let arrangements = s.inst.root.join("Cactus/arrangements/ADMBase");
    assert!(arrangements.join("StaticConformal/interface.ccl").is_file());
    assert!(arrangements.join("ADMCoupling/interface.ccl").is_file(), "the enabled thorn stays");
    let state = crate::fetch::FetchState::read(&s.inst.root).unwrap().unwrap();
    assert_eq!(state.repos["admbase"].head, it.a2);
    let _ = &it.a1;
}

/// Every pinned repo of the base goes out in one `/lookup` request (the
/// endpoint is rate-limited), and a `commit-known` of null (the server could
/// not check its mirror) still maps the repo: nothing reads it as unpushed.
#[test]
fn the_base_is_looked_up_in_one_request_and_null_commit_known_maps() {
    let tmp = tempfile::tempdir().unwrap();
    let mut base = String::new();
    let mut urls = Vec::new();
    for name in ["one", "two", "three", "four", "five"] {
        let arrangement = format!("Arr{name}");
        let (url, c) = upstream(tmp.path(), name, &[(&format!("T{name}/interface.ccl"), "IMPLEMENTS: x\n")]);
        base.push_str(&section(&url, &c, &[&format!("{arrangement}/T{name}")]));
        urls.push((url, c));
    }
    let (aurl, a) = (urls[0].0.clone(), urls[0].1.clone());
    let (fake, log) = Fake::new()
        .package("et/one", vec![version("1.0.0", &a, &aurl, "Arrone", &["Tone", "Extra"], "[]")])
        .pin(&aurl, "et/one", Some("1.0.0"))
        .serve();
    let s = Setup::new(tmp, &base, &fake);
    // Naming a base package: it is in the base already.
    let got = records(&s, &["et/one/Extra"], &AddFlags::default()).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(record_of(&got, "et/one").2, ["Arrone/Extra"]);
    let lookups: Vec<String> =
        log.lock().unwrap().iter().filter(|(p, _)| p.ends_with("/lookup")).map(|(_, b)| b.clone()).collect();
    assert_eq!(lookups.len(), 1, "{lookups:?}");
    let asked: cactup_ecosystem_common::api::LookupRequest = serde_json::from_str(&lookups[0]).unwrap();
    assert_eq!(asked.pins.len(), 5);
    assert!(urls.iter().all(|(u, c)| asked.pins.iter().any(|p| &p.url == u && &p.commit == c)));
}

/// More thorns of a base package go into an `[[added]]` entry of their own,
/// at the base's commit; an `added-for` entry an older binary's `remove` left
/// for it (saying it was brought in, for that very commit) is not inherited:
/// a package with no `[[added]]` entry before starts as named.
#[test]
fn a_leftover_added_for_entry_is_not_inherited() {
    let it = admbase("^1.0");
    let s = &it.setup;
    let mut meta = s.meta();
    let leftover = AddedFor { requested: false, by: vec!["max/old@1.0.0".into()], for_commit: Some(it.a1.clone()), choices: vec![] };
    meta.added_for.insert("et/admbase".into(), leftover);
    s.write_meta(&meta);
    s.add(&["max/app"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    let meta = s.meta();
    let admbase = meta.added.iter().find(|a| a.package == "et/admbase").unwrap();
    assert_eq!((admbase.commit.as_str(), admbase.repo.as_str()), (it.a1.as_str(), "admbase"));
    let entry = &meta.added_for["et/admbase"];
    assert!(entry.requested, "{entry:?}");
    assert!(!entry.by.contains(&"max/old@1.0.0".to_owned()), "{entry:?}");
    assert!(s.inst.root.join("Cactus/arrangements/ADMBase/StaticConformal/interface.ccl").is_file());
    let _ = (&it.a2, &it.log);
}

/// A plan with a flesh step is refused, before anything is shown or
/// changed: this cactup does not replace the flesh yet.
#[test]
fn a_flesh_step_is_refused() {
    let flesh = version_with("1.0.0", &"a".repeat(40), "https://x/cactus.git", &[], "[]", true);
    let (base, _) = Fake::new().package("et/cactus", vec![flesh]).serve();
    let tmp = tempfile::tempdir().unwrap();
    let s = Setup::new(tmp, "", &base);
    let err = format!("{:#}", s.add(&["et/cactus"], yes()).unwrap_err());
    assert!(err.contains("Cactus flesh") && err.contains("cannot do yet"), "{err}");
    assert!(s.meta().added.is_empty());
}

/// A thorn taken over from a base repo is taken at the base's own
/// `Arrangement/Thorn`: a fork that keeps it in another arrangement is
/// refused, since compose would keep both.
#[test]
fn a_replaced_thorn_must_keep_the_base_s_checkout() {
    use crate::deps::{PkgId, Plan, Replaced, Step};
    let base = thornlist::parse(&format!(
        "!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n{}",
        section("https://x/carpetx.git", &"c".repeat(40), &["CarpetX/CarpetX", "CarpetX/Loop"])
    ))
    .unwrap();
    let fork = |arrangement: &str| AddedPackage {
        package: "max/carpetx-mp".into(),
        version: "1.0.0".into(),
        tag: None,
        commit: "d".repeat(40),
        url: "https://x/carpetx-mp.git".into(),
        repo: "carpetx-mp".into(),
        thorns: vec![AddedThorn { checkout: format!("{arrangement}/CarpetX"), repo_path: "$2".into() }],
        replaces: vec!["carpetx".into()],
        contact: None,
        libraries: Vec::new(),
    };
    let replaced = Replaced::Base { repo: "carpetx".into() };
    let by = PkgId::parse("max/carpetx-mp").unwrap();
    let step = Step::Replace { replaced, by, thorns: vec!["CarpetX".into()], flesh: false, explicit: true };
    let plan = Plan { steps: vec![step], ..Plan::default() };
    let meta = InstallationMeta::default();
    place(&base, &meta, vec![fork("CarpetX")], &plan, &mut Vec::new()).unwrap();
    let err = format!("{:#}", place(&base, &meta, vec![fork("CarpetXMP")], &plan, &mut Vec::new()).unwrap_err());
    assert!(err.contains("CarpetX/CarpetX") && err.contains("same Arrangement/Thorn"), "{err}");
}

/// The CCL check reads the installation's own thorns from the tree: a base
/// repo no registered package is (looked up as nothing) meets a new thorn's
/// requirement by what its `interface.ccl` says on disk; when it does not,
/// the add is refused, listing what `/providers` offers as `cactup add`
/// lines (a provider on another track named at its version there).
#[test]
fn the_ccl_check_reads_present_thorns_from_the_tree() {
    for meets in [true, false] {
        let tmp = tempfile::tempdir().unwrap();
        let implements = if meets { "mylib" } else { "other" };
        let (lurl, l) = upstream(tmp.path(), "mylib", &[("MyLib/interface.ccl", &format!("IMPLEMENTS: {implements}\n"))]);
        let (zurl, z) = upstream(tmp.path(), "z4c", &[("Z4c/interface.ccl", "IMPLEMENTS: z4c\nINHERITS: mylib\n")]);
        let facts = ThornFacts { implements: Some("z4c".into()), inherits: vec!["mylib".into()], ..ThornFacts::default() };
        let z4c = version_with("1.0.0", &z, &zurl, &[thorn("Z4c", "Z4c", &facts)], "[]", false);
        let mut fake = Fake::new().package("max/z4c", vec![z4c]);
        fake.providers =
            r#"{"providers":[{"package":"et/mylib","version":"mp:1.0.0","arrangement":"MyLib","thorn":"MyLib"}]}"#.into();
        let (base, _) = fake.serve();
        let s = Setup::new(tmp, &section(&lurl, &l, &["MyLib/MyLib"]), &base);
        let result = s.add(&["max/z4c"], yes());
        if meets {
            result.unwrap_or_else(|e| panic!("{e:#}"));
            assert_eq!(s.meta().added.len(), 1);
        } else {
            let err = format!("{:#}", result.unwrap_err());
            assert!(err.contains("Z4c (max/z4c)") && err.contains("mylib"), "{err}");
            assert!(err.contains("cactup add et/mylib/MyLib@mp:1.0.0"), "{err}");
            assert!(s.meta().added.is_empty());
        }
    }
}
