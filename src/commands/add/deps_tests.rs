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
/// answers with. `lookup`/`providers` false: an older Cactarium, which has
/// no such endpoint (its API fallback answers 404, with its generation).
#[derive(Clone)]
struct Fake {
    packages: Vec<(String, Vec<String>)>,
    pins: Vec<(String, String, Option<String>)>,
    providers: String,
    lookup: bool,
    providers_endpoint: bool,
    /// Reads at a commit no version froze so far: the real server reads the
    /// package now (a fetch of its repository), at most 12 in a window that
    /// opens at the first counted read, then answers 429 (refused reads do
    /// not count).
    unfrozen_reads: usize,
    /// Reads in the current window, and when it opened.
    in_window: usize,
    window_start: Option<std::time::Instant>,
    /// The window's length (60 s on the real server).
    window: std::time::Duration,
    /// Whether a 429 says `Retry-After`.
    retry_after: bool,
    /// Every read at an unfrozen commit answers 429.
    always_limited: bool,
    /// Commits whose reads answer 403 (not served to this caller).
    refuse_at: Vec<String>,
    /// What a read at a commit no version froze answers (the package read
    /// now), by package and commit.
    snapshots: Vec<(String, String, String)>,
}

/// A fake Cactarium's state, changeable while it serves.
type Shared = Arc<Mutex<Fake>>;

impl Fake {
    fn new() -> Fake {
        Fake {
            packages: Vec::new(),
            pins: Vec::new(),
            providers: r#"{"providers":[]}"#.to_owned(),
            lookup: true,
            providers_endpoint: true,
            unfrozen_reads: 0,
            in_window: 0,
            window_start: None,
            window: std::time::Duration::from_secs(60),
            retry_after: false,
            always_limited: false,
            refuse_at: Vec::new(),
            snapshots: Vec::new(),
        }
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
        let (base, log, _) = self.serve_shared();
        (base, log)
    }

    fn serve_shared(self) -> (String, Log, Shared) {
        let log: Log = Arc::default();
        let seen = log.clone();
        let state: Shared = Arc::new(Mutex::new(self));
        let fake = state.clone();
        let (base, _) = crate::fetch::download::test_server::serve_with_body(1000, move |path, body| {
            seen.lock().unwrap().push((path.to_owned(), String::from_utf8_lossy(body).into_owned()));
            fake.lock().unwrap().answer(path, body)
        });
        (base, log, state)
    }

    fn answer(&mut self, path: &str, body: &[u8]) -> (&'static str, String, Vec<u8>) {
        let ok = |body: String| ("200 OK", "api-generation: 1\r\nx-cactarium-untrusted: false\r\n".to_owned(), body.into_bytes());
        let missing = || {
            let body = r#"{"error":{"code":"not-found","message":"No such package"}}"#;
            ("404 Not Found", "api-generation: 1\r\n".to_owned(), body.as_bytes().to_vec())
        };
        // The real server's API fallback: 404, with its API generation.
        let no_route = || {
            let body = r#"{"error":{"code":"not-found","message":"Not found"}}"#;
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
            if !self.lookup {
                return no_route();
            }
            let asked: cactup_ecosystem_common::api::LookupRequest = serde_json::from_slice(body).unwrap();
            let pins: Vec<String> = (asked.pins.iter())
                .map(|p| {
                    let known = self.pins.iter().find(|(url, _, _)| *url == p.url);
                    let package = known.map_or("null".to_owned(), |(_, pkg, _)| format!("\"{pkg}\""));
                    // The version that froze the commit, as the server says:
                    // null when none did.
                    let version = known
                        .and_then(|(_, pkg, _)| self.frozen(pkg, &p.commit))
                        .map_or("null".to_owned(), |v| format!("\"{v}\""));
                    // The real server often cannot say whether its mirror has
                    // the commit: null.
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
            return if self.providers_endpoint { ok(self.providers.clone()) } else { no_route() };
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
            if self.refuse_at.iter().any(|c| c == at) {
                let body = r#"{"error":{"code":"forbidden","message":"not served: the package isn't trusted"}}"#;
                return ("403 Forbidden", "api-generation: 1\r\n".to_owned(), body.as_bytes().to_vec());
            }
            // Several versions at one commit (tracks): this fake answers the
            // last listed, as a server might any of them.
            if let Some(v) = versions.iter().rev().find(|v| v.contains(&format!("\"commit\":\"{at}"))) {
                return ok(v.clone());
            }
            let now = std::time::Instant::now();
            if self.window_start.is_some_and(|s| now.duration_since(s) >= self.window) {
                (self.window_start, self.in_window) = (None, 0);
            }
            if self.always_limited || self.in_window >= 12 {
                let body = r#"{"error":{"code":"rate-limited","message":"Too many requests; try again in a minute"}}"#;
                let left = self.window_start.map_or(self.window, |s| self.window.saturating_sub(now.duration_since(s)));
                let header = if self.retry_after { format!("retry-after: {}\r\n", left.as_secs().max(1)) } else { String::new() };
                return ("429 Too Many Requests", format!("api-generation: 1\r\n{header}"), body.as_bytes().to_vec());
            }
            self.window_start.get_or_insert(now);
            self.in_window += 1;
            self.unfrozen_reads += 1;
            return match self.snapshots.iter().find(|(p, c, _)| p == id && c == at) {
                Some((_, _, json)) => ok(json.clone()),
                None => missing(),
            };
        }
        no_route()
    }
}

impl Fake {
    /// The version of `package` that froze `commit`, if one did.
    /// On the default track first, as the server's lookup answers.
    fn frozen(&self, package: &str, commit: &str) -> Option<String> {
        let (_, versions) = self.packages.iter().find(|(p, _)| p == package)?;
        let label = |v: &String| {
            let start = v.find("\"version\":\"")? + "\"version\":\"".len();
            Some(v[start..].split('"').next()?.to_owned())
        };
        let at: Vec<String> =
            versions.iter().filter(|v| v.contains(&format!("\"commit\":\"{commit}\""))).filter_map(label).collect();
        at.iter().find(|v| !v.contains(':')).or(at.first()).cloned()
    }
}

/// The requests the fake got, by kind: `/resolve`, `/lookup`, whole
/// version lists, one version at a commit, `/providers`.
fn counts(log: &Log) -> [usize; 5] {
    let mut n = [0; 5];
    for (path, _) in log.lock().unwrap().iter() {
        let path = path.strip_prefix("/api/v1").unwrap_or(path);
        let i = match path {
            p if p.starts_with("/resolve") => 0,
            "/lookup" => 1,
            p if p.ends_with("/versions") => 2,
            p if p.contains("/versions/") => 3,
            _ => 4,
        };
        n[i] += 1;
    }
    n
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
    fake: Shared,
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
    let (base, log, fake) = Fake::new()
        .package("max/z4c", vec![version("1.0.0", &z, &zurl, "Z4c", &["Z4c"], &claims)])
        .package("et/carpetx", vec![version("1.0.0", &c, &curl, "CarpetX", &["CarpetX", "Loop", "Arith"], "[]")])
        .serve_shared();
    Z4c { setup: Setup::new(tmp, "", &base), z, c, log, fake }
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
/// StaticConformal and ADMMacros, and 2.0.0 a commit later; max/app 1.0.0
/// needs StaticConformal of et/admbase `range`, and max/app2 1.0.0 needs its
/// ADMBase and ADMMacros at ^1.0. max/solo 1.0.0 needs nothing.
struct Admbase {
    setup: Setup,
    a1: String,
    a2: String,
    log: Log,
    fake: Shared,
}

fn admbase(range: &str) -> Admbase {
    admbase_with(range, |f| f)
}

fn admbase_with(range: &str, change: impl FnOnce(Fake) -> Fake) -> Admbase {
    admbase_full(range, true, true, change)
}

/// [`admbase`], with the base at a1 no version froze: 1.0.0 froze another
/// commit, and a read at a1 answers the package read there.
fn admbase_unfrozen(range: &str) -> Admbase {
    admbase_full(range, true, false, |f| f)
}

/// [`admbase_with`], its base section pinned or following its branch.
fn admbase_full(range: &str, pinned: bool, frozen: bool, change: impl FnOnce(Fake) -> Fake) -> Admbase {
    let tmp = tempfile::tempdir().unwrap();
    let files = [
        ("ADMBase/interface.ccl", "IMPLEMENTS: admbase\n"),
        ("ADMCoupling/interface.ccl", "IMPLEMENTS: admcoupling\n"),
        ("StaticConformal/interface.ccl", "IMPLEMENTS: staticconformal\n"),
        ("ADMMacros/interface.ccl", "IMPLEMENTS: admmacros\n"),
    ];
    let (aurl, a1) = upstream(tmp.path(), "admbase", &files);
    let (_, a2) = upstream(tmp.path(), "admbase", &[files[0], files[1], files[2], files[3], ("README", "2\n")]);
    let (purl, p) = upstream(tmp.path(), "app", &[("App/interface.ccl", "IMPLEMENTS: app\n")]);
    let (p2url, p2) = upstream(tmp.path(), "app2", &[("App2/interface.ccl", "IMPLEMENTS: app2\n")]);
    let (surl, so) = upstream(tmp.path(), "solo", &[("Solo/interface.ccl", "IMPLEMENTS: solo\n")]);
    let (p3url, p3) = upstream(tmp.path(), "app3", &[("App3/interface.ccl", "IMPLEMENTS: app3\n")]);
    let claims3 = format!("[{}]", depends("et/admbase", ">=1.0", &["ADMMacros"]));
    let thorns = ["ADMBase", "ADMCoupling", "StaticConformal", "ADMMacros"];
    let claims = format!("[{}]", depends("et/admbase", range, &["StaticConformal"]));
    let claims2 = format!("[{}]", depends("et/admbase", "^1.0", &["ADMBase", "ADMMacros"]));
    let one = if frozen { a1.clone() } else { "1".repeat(40) };
    let mut fake = Fake::new();
    if !frozen {
        let read = version(&a1, &a1, &aurl, "ADMBase", &thorns, "[]");
        fake.snapshots.push(("et/admbase".to_owned(), a1.clone(), read));
    }
    let fake = fake
        .package("max/app", vec![version("1.0.0", &p, &purl, "App", &["App"], &claims)])
        .package("max/app2", vec![version("1.0.0", &p2, &p2url, "App2", &["App2"], &claims2)])
        .package("max/solo", vec![version("1.0.0", &so, &surl, "Solo", &["Solo"], "[]")])
        .package("max/app3", vec![version("1.0.0", &p3, &p3url, "App3", &["App3"], &claims3)])
        .package(
            "et/admbase",
            vec![
                version("2.0.0", &a2, &aurl, "ADMBase", &thorns, "[]"),
                version("1.0.0", &one, &aurl, "ADMBase", &thorns, "[]"),
            ],
        )
        .pin(&aurl, "et/admbase", Some("1.0.0"));
    let (base, log, fake) = change(fake).serve_shared();
    let mut text = section(&aurl, &a1, &["ADMBase/ADMBase", "ADMBase/ADMCoupling"]);
    if !pinned {
        text = text.replace(&format!("!REPO_COMMIT = {a1}\n"), "");
    }
    let setup = Setup::new(tmp, &text, &base);
    Admbase { setup, a1, a2, log, fake }
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
    place(&base, &meta, records, &plan, &mut Vec::new(), &on_disk(&s.inst.root, base.root()).unwrap())
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
    let got = records(s, &["et/admbase/StaticConformal"], &upgrade).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(sorted(record_of(&got, "et/admbase")), want);
    let replace = AddFlags { replace: true, ..AddFlags::default() };
    let got = records(s, &["et/admbase/StaticConformal@2.0.0"], &replace).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(sorted(record_of(&got, "et/admbase")), want);

    // And on disk: the base's clone moves to 2.0.0 with those three thorns.
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
    // Base repo one is et/one at 1.0.0, read at its commit; the rest are no
    // registered package's.
    let pristine = std::fs::read_to_string(s.inst.source_thornlist()).unwrap();
    let list = thornlist::parse(&pristine).unwrap();
    let client = crate::cactarium::Client::new(&s.ctx.db.read().unwrap());
    let mut reg = super::super::resolution::Registry::new(&client, []);
    let read = super::super::resolution::present(&mut reg, &s.inst.root, &list, &[], None, &Default::default());
    reg.finish();
    let read = read.unwrap_or_else(|e| panic!("{e:#}"));
    let ids: Vec<(String, String)> = (read.present.iter())
        .map(|p| (p.release.id.to_string(), p.release.label()))
        .collect();
    assert!(ids.contains(&("et/one".to_owned(), "et/one@1.0.0".to_owned())), "{ids:?}");
    assert_eq!(ids.iter().filter(|(id, _)| id.starts_with("repo:")).count(), 4, "{ids:?}");
    // Naming a base package: it is in the base already, so its new thorn
    // goes into the base's own clone.
    log.lock().unwrap().clear();
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
/// at the base's commit, brought in (not named); an `added-for` entry an
/// older binary's `remove` left for it (saying it was named, for that very
/// commit, by another dependent) is not inherited: a package with no
/// `[[added]]` entry before starts afresh.
#[test]
fn a_leftover_added_for_entry_is_not_inherited() {
    let it = admbase("^1.0");
    let s = &it.setup;
    let mut meta = s.meta();
    let leftover = AddedFor { requested: true, by: vec!["max/old@1.0.0".into()], for_commit: Some(it.a1.clone()), choices: vec![] };
    meta.added_for.insert("et/admbase".into(), leftover);
    s.write_meta(&meta);
    s.add(&["max/app"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    let meta = s.meta();
    let admbase = meta.added.iter().find(|a| a.package == "et/admbase").unwrap();
    assert_eq!((admbase.commit.as_str(), admbase.repo.as_str()), (it.a1.as_str(), "admbase"));
    let entry = &meta.added_for["et/admbase"];
    assert!(!entry.requested, "{entry:?}");
    assert_eq!(entry.by, ["max/app@1.0.0"], "{entry:?}");
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
    place(&base, &meta, vec![fork("CarpetX")], &plan, &mut Vec::new(), &Default::default()).unwrap();
    let err = format!("{:#}", place(&base, &meta, vec![fork("CarpetXMP")], &plan, &mut Vec::new(), &Default::default()).unwrap_err());
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

/// A remaining added package whose release the Cactarium no longer serves
/// (unapproved since, renamed, a commit it cannot find) hides what it
/// depends on: `remove` refuses rather than delete something it may need,
/// naming it and `--no-deps`.
#[test]
fn remove_refuses_while_a_dependent_cannot_be_read() {
    let it = z4c();
    let s = &it.setup;
    s.add(&["max/z4c"], yes()).unwrap();
    it.fake.lock().unwrap().packages.retain(|(p, _)| p != "max/z4c");
    let err = format!("{:#}", s.remove(&["et/carpetx"], remove_yes()).unwrap_err());
    assert!(err.contains("max/z4c") && err.contains("--no-deps"), "{err}");
    assert_eq!(s.meta().added.len(), 2, "nothing removed");
    s.remove(&["et/carpetx"], RemoveFlags { no_deps: true, ..RemoveFlags::default() }).unwrap();
    assert_eq!(s.meta().added.len(), 1);
}

/// `add` beside an added package whose release cannot be read: a plan that
/// moves a present package (or any plan under `--upgrade`) is refused, as
/// that package's ranges are unknown; one that moves nothing goes ahead.
#[test]
fn add_refuses_to_move_packages_beside_one_that_cannot_be_read() {
    let it = admbase("^2.0");
    let s = &it.setup;
    s.add(&["max/solo"], yes()).unwrap();
    it.fake.lock().unwrap().packages.retain(|(p, _)| p != "max/solo");
    let upgrade = AddFlags { upgrade: true, ..AddFlags::default() };
    let err = format!("{:#}", records(s, &["max/app"], &upgrade).unwrap_err());
    assert!(err.contains("max/solo") && err.contains("--no-deps"), "{err}");
    // Nothing moves: a warning, and on.
    records(s, &["max/app2"], &AddFlags::default()).unwrap_or_else(|e| panic!("{e:#}"));
    let _ = (&it.a1, &it.a2, &it.log);
}

/// After a dependency gave base package et/admbase another thorn, the base
/// repo and that `[[added]]` entry are one package: a second dependent that
/// needs a base thorn (ADMBase) and one more (ADMMacros) gets them from it,
/// no clash, and the entry holds both added thorns.
#[test]
fn a_second_dependent_of_a_base_package_s_thorns_finds_them() {
    let it = admbase("^1.0");
    let s = &it.setup;
    s.add(&["max/app"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    s.add(&["max/app2"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    let added = s.added();
    let admbase = added.iter().find(|(p, _)| p == "et/admbase").unwrap();
    let mut thorns = admbase.1.clone();
    thorns.sort();
    assert_eq!(thorns, ["ADMBase/ADMMacros", "ADMBase/StaticConformal"]);
    assert!(added.iter().any(|(p, _)| p == "max/app2"));
    assert_eq!(s.meta().added.iter().find(|a| a.package == "et/admbase").unwrap().commit, it.a1);
    let _ = (&it.a2, &it.log, &it.fake);
}

/// Thorns of a base package brought in only for a dependency are recorded as
/// brought in, and go once the dependent goes: only those thorns leave the
/// thornlist, the base repo stays. While something needs them they cannot
/// be removed. Named, they stay.
#[test]
fn base_thorns_brought_in_for_a_dependency_go_with_it() {
    let it = admbase("^1.0");
    let s = &it.setup;
    s.add(&["max/app"], yes()).unwrap();
    assert!(!s.meta().added_for["et/admbase"].requested);
    let err = format!("{:#}", s.remove(&["et/admbase"], remove_yes()).unwrap_err());
    assert!(err.contains("max/app@1.0.0"), "{err}");
    s.remove(&["max/app"], remove_yes()).unwrap_or_else(|e| panic!("{e:#}"));
    assert!(s.meta().added.is_empty(), "{:?}", s.added());
    let live = std::fs::read_to_string(s.inst.live_thornlist().unwrap()).unwrap();
    assert!(!live.contains("StaticConformal") && live.contains("ADMBase/ADMCoupling"), "{live}");

    // Named, the thorn stays when the package that needed it goes.
    s.add(&["max/app", "et/admbase/StaticConformal"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    assert!(s.meta().added_for["et/admbase"].requested);
    s.remove(&["max/app"], remove_yes()).unwrap();
    assert_eq!(s.added(), [("et/admbase".to_owned(), vec!["ADMBase/StaticConformal".to_owned()])]);
    let _ = (&it.a1, &it.a2, &it.log, &it.fake);
}

/// A Cactarium older than this cactup, with no `/lookup`: `add` says so and
/// names `--no-deps`, which then adds exactly what was named, the base taken
/// as repos of no known package.
#[test]
fn an_older_cactarium_without_lookup() {
    let it = admbase_with("^1.0", |mut f| {
        f.lookup = false;
        f
    });
    let s = &it.setup;
    let err = format!("{:#}", s.add(&["max/app"], yes()).unwrap_err());
    assert!(err.contains("older than this cactup") && err.contains("--no-deps"), "{err}");
    s.add(&["max/app"], AddFlags { no_deps: true, ..AddFlags::default() }).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(s.added(), [("max/app".to_owned(), vec!["App/App".to_owned()])]);
    let _ = (&it.a1, &it.a2, &it.log, &it.fake);
}

/// The requests `add` and `remove` make on a base of five registered
/// packages: an unmoved present package is read at its commit (one small
/// answer), and whole version lists are asked for only what may move.
#[test]
fn the_requests_add_and_remove_make() {
    let tmp = tempfile::tempdir().unwrap();
    let mut base = String::new();
    let mut fake = Fake::new();
    for name in ["one", "two", "three", "four", "five"] {
        let (url, c) = upstream(tmp.path(), name, &[(&format!("T{name}/interface.ccl"), "IMPLEMENTS: x\n")]);
        base.push_str(&section(&url, &c, &[&format!("Arr{name}/T{name}")]));
        let id = format!("et/{name}");
        fake = fake.package(&id, vec![version("1.0.0", &c, &url, &format!("Arr{name}"), &[&format!("T{name}")], "[]")]);
        fake = fake.pin(&url, &id, Some("1.0.0"));
    }
    let (nurl, n) = upstream(tmp.path(), "new", &[("New/interface.ccl", "IMPLEMENTS: new\n")]);
    let (durl, d) = upstream(tmp.path(), "dep", &[("Dep/interface.ccl", "IMPLEMENTS: dep\n")]);
    let (lurl, l) = upstream(tmp.path(), "lib", &[("Lib/interface.ccl", "IMPLEMENTS: lib\n")]);
    let claims = format!("[{}]", depends("et/lib", "^1.0", &[]));
    let (cactarium, log) = fake
        .package("max/new", vec![version("1.0.0", &n, &nurl, "New", &["New"], "[]")])
        .package("max/dep", vec![version("1.0.0", &d, &durl, "Dep", &["Dep"], &claims)])
        .package("et/lib", vec![version("1.0.0", &l, &lurl, "Lib", &["Lib"], "[]")])
        .serve();
    let s = Setup::new(tmp, &base, &cactarium);
    let count = |specs: &[&str], flags: AddFlags| {
        log.lock().unwrap().clear();
        records(&s, specs, &flags).unwrap_or_else(|e| panic!("{e:#}"));
        counts(&log)
    };
    // [resolve, lookup, whole version lists, one version at a commit, other]
    assert_eq!(count(&["max/new"], AddFlags::default()), [1, 1, 1, 5, 0]);
    assert_eq!(count(&["max/dep"], AddFlags::default()), [1, 1, 2, 5, 0]);
    assert_eq!(count(&["max/dep"], AddFlags { upgrade: true, ..AddFlags::default() }), [1, 1, 7, 0, 0]);
    assert_eq!(count(&["max/dep"], AddFlags { upgrade_all: true, ..AddFlags::default() }), [1, 1, 7, 0, 0]);
    s.add(&["max/new"], yes()).unwrap();
    log.lock().unwrap().clear();
    s.remove(&["max/new"], remove_yes()).unwrap();
    assert_eq!(counts(&log), [0, 1, 0, 6, 0]);
}

/// The live thornlist is read before the prompt; hand edits made while it
/// waits are not overwritten: the check under the lock sees them.
#[test]
fn hand_edits_made_while_the_prompt_waits_are_caught() {
    let it = z4c();
    let s = &it.setup;
    let start = Start::read(&s.ctx, false, Doing::Adding).unwrap();
    let live = s.inst.live_thornlist().unwrap();
    let text = std::fs::read_to_string(&live).unwrap();
    std::fs::write(&live, format!("{text}\n!TARGET = $ROOT/arrangements\n!TYPE = ignore\n!CHECKOUT = Mine/Mine\n")).unwrap();
    let locked = start.inst.locked().unwrap();
    let err = format!("{:#}", start.recheck(&locked).unwrap_err());
    assert!(err.contains("changed while"), "{err}");
    let _ = (&it.z, &it.c, &it.log, &it.fake);
}

/// The check that a replaced thorn keeps the base's `Arrangement/Thorn` runs
/// before anything is confirmed: `--dry-run` already refuses.
#[test]
fn a_replaced_checkout_is_checked_before_the_prompt() {
    let tmp = tempfile::tempdir().unwrap();
    let (curl, c) = upstream(tmp.path(), "carpetx", &[("CarpetX/interface.ccl", "IMPLEMENTS: carpetx\n")]);
    let (furl, f) = upstream(tmp.path(), "carpetx-mp", &[("CarpetX/interface.ccl", "IMPLEMENTS: carpetx\n")]);
    let (cactarium, _) = Fake::new()
        .package("max/carpetx-mp", vec![version("1.0.0", &f, &furl, "CarpetXMP", &["CarpetX"], "[]")])
        .serve();
    let s = Setup::new(tmp, &section(&curl, &c, &["CarpetX/CarpetX"]), &cactarium);
    let flags = AddFlags { replace: true, dry_run: true, ..AddFlags::default() };
    let err = format!("{:#}", s.add(&["max/carpetx-mp"], flags).unwrap_err());
    assert!(err.contains("same Arrangement/Thorn"), "{err}");
}

/// No `/providers` on an older Cactarium: the unmet requirement is still the
/// refusal, saying the Cactarium cannot list what provides it.
#[test]
fn an_older_cactarium_without_providers_keeps_the_refusal() {
    let tmp = tempfile::tempdir().unwrap();
    let (lurl, l) = upstream(tmp.path(), "mylib", &[("MyLib/interface.ccl", "IMPLEMENTS: other\n")]);
    let (zurl, z) = upstream(tmp.path(), "z4c", &[("Z4c/interface.ccl", "IMPLEMENTS: z4c\nINHERITS: mylib\n")]);
    let facts = ThornFacts { implements: Some("z4c".into()), inherits: vec!["mylib".into()], ..ThornFacts::default() };
    let z4c = version_with("1.0.0", &z, &zurl, &[thorn("Z4c", "Z4c", &facts)], "[]", false);
    let mut fake = Fake::new().package("max/z4c", vec![z4c]);
    fake.providers_endpoint = false;
    let (base, _) = fake.serve();
    let s = Setup::new(tmp, &section(&lurl, &l, &["MyLib/MyLib"]), &base);
    let err = format!("{:#}", s.add(&["max/z4c"], yes()).unwrap_err());
    assert!(err.contains("Z4c (max/z4c)") && err.contains("mylib"), "{err}");
    assert!(err.contains("cannot list"), "{err}");
    assert!(!err.contains("no registered package provides it"), "{err}");
}

/// Without its pristine thornlist an installation has nothing to add to.
#[test]
fn add_needs_the_pristine_thornlist() {
    let it = z4c();
    let s = &it.setup;
    std::fs::remove_file(s.inst.source_thornlist()).unwrap();
    let err = format!("{:#}", s.add(&["max/z4c"], yes()).unwrap_err());
    assert!(err.contains("no pristine thornlist to add to"), "{err}");
    let _ = (&it.z, &it.c, &it.log, &it.fake);
}

/// A base that follows branches: twenty registered repos at commits no
/// version froze. Reading a package at such a commit makes the server read it
/// now (a fetch of its repository, 12 a minute): none is read. `add`, `add
/// --no-deps`, `add --upgrade` and `remove` all go through, each with a
/// handful of requests.
#[test]
fn a_base_at_unfrozen_commits_is_never_read_at_them() {
    let tmp = tempfile::tempdir().unwrap();
    let mut base = String::new();
    let mut fake = Fake::new();
    for i in 0..20 {
        let name = format!("r{i}");
        let (url, c) = upstream(tmp.path(), &name, &[(&format!("T{i}/interface.ccl"), "IMPLEMENTS: x\n")]);
        base.push_str(&section(&url, &c, &[&format!("Arr{i}/T{i}")]));
        let id = format!("et/{name}");
        // Its one version froze another commit.
        let other = format!("{:040x}", i + 1);
        fake = fake.package(&id, vec![version("1.0.0", &other, &url, &format!("Arr{i}"), &[&format!("T{i}")], "[]")]);
        fake = fake.pin(&url, &id, None);
    }
    let (nurl, n) = upstream(tmp.path(), "new", &[("New/interface.ccl", "IMPLEMENTS: new\n")]);
    let (cactarium, log, fake) =
        fake.package("max/new", vec![version("1.0.0", &n, &nurl, "New", &["New"], "[]")]).serve_shared();
    let s = Setup::new(tmp, &base, &cactarium);
    let count = |specs: &[&str], flags: AddFlags| {
        log.lock().unwrap().clear();
        records(&s, specs, &flags).unwrap_or_else(|e| panic!("{e:#}"));
        counts(&log)
    };
    // [resolve, lookup, whole version lists, one version at a commit, other]
    assert_eq!(count(&["max/new"], AddFlags::default()), [1, 1, 1, 0, 0]);
    assert_eq!(count(&["max/new"], AddFlags { no_deps: true, ..AddFlags::default() }), [1, 1, 1, 0, 0]);
    assert_eq!(count(&["max/new"], AddFlags { upgrade: true, ..AddFlags::default() }), [1, 1, 21, 0, 0]);
    s.add(&["max/new"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    log.lock().unwrap().clear();
    s.remove(&["max/new"], remove_yes()).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(counts(&log), [0, 1, 0, 1, 0]);
    assert_eq!(fake.lock().unwrap().unfrozen_reads, 0);
}

/// A base package's frozen release that the Cactarium will not serve now
/// (403: not served to this caller; 429 alike) leaves its dependencies
/// unknown, no failure; replacing it is no move beside an unread package.
#[test]
fn a_base_read_refused_leaves_it_unchecked() {
    let it = admbase("^1.0");
    let s = &it.setup;
    it.fake.lock().unwrap().refuse_at.push(it.a1.clone());
    let replace = AddFlags { replace: true, ..AddFlags::default() };
    let got = records(s, &["et/admbase/StaticConformal@2.0.0"], &replace).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(record_of(&got, "et/admbase").0, "2.0.0");
    records(s, &["max/solo"], &AddFlags::default()).unwrap_or_else(|e| panic!("{e:#}"));
    let _ = (&it.a2, &it.log);
}

/// A registered fork in the base (max/carpet-mp, which stands in for
/// et/carpet) given CarpetSlab for max/d: the extension is held through the
/// fork, so it cannot be removed while max/d needs it, nor swept as an
/// orphan when something unrelated goes.
#[test]
fn a_base_extension_held_through_a_fork_stays() {
    let tmp = tempfile::tempdir().unwrap();
    let (furl, f) = upstream(
        tmp.path(),
        "carpet-mp",
        &[("Carpet/interface.ccl", "IMPLEMENTS: driver\n"), ("CarpetSlab/interface.ccl", "IMPLEMENTS: hyperslab\n")],
    );
    let (durl, d) = upstream(tmp.path(), "d", &[("D/interface.ccl", "IMPLEMENTS: d\n")]);
    let (surl, so) = upstream(tmp.path(), "solo", &[("Solo/interface.ccl", "IMPLEMENTS: solo\n")]);
    let fork = r#"[{"kind":"fork-of","target":"et/carpet","range":"=2026.5.0","note":null}]"#;
    let claims = format!("[{}]", depends("et/carpet", "^2026.5", &["CarpetSlab"]));
    let (cactarium, _) = Fake::new()
        .package("max/carpet-mp", vec![version("1.0.0", &f, &furl, "Carpet", &["Carpet", "CarpetSlab"], fork)])
        .package("et/carpet", vec![version("2026.5.0", &"e".repeat(40), "https://x/carpet.git", "Carpet", &["Carpet", "CarpetSlab"], "[]")])
        .package("max/d", vec![version("1.0.0", &d, &durl, "D", &["D"], &claims)])
        .package("max/solo", vec![version("1.0.0", &so, &surl, "Solo", &["Solo"], "[]")])
        .pin(&furl, "max/carpet-mp", None)
        .serve();
    let s = Setup::new(tmp, &section(&furl, &f, &["Carpet/Carpet"]), &cactarium);
    s.add(&["max/d"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    assert!(s.added().contains(&("max/carpet-mp".to_owned(), vec!["Carpet/CarpetSlab".to_owned()])), "{:?}", s.added());
    assert!(!s.meta().added_for["max/carpet-mp"].requested);

    let err = format!("{:#}", s.remove(&["max/carpet-mp"], remove_yes()).unwrap_err());
    assert!(err.contains("max/d@1.0.0"), "{err}");
    s.add(&["max/solo"], yes()).unwrap();
    s.remove(&["max/solo"], remove_yes()).unwrap_or_else(|e| panic!("{e:#}"));
    assert!(s.added().iter().any(|(p, _)| p == "max/carpet-mp"), "{:?}", s.added());
    // max/d gone, nothing needs it: it goes along.
    s.remove(&["max/d"], remove_yes()).unwrap_or_else(|e| panic!("{e:#}"));
    assert!(s.added().is_empty(), "{:?}", s.added());
}

/// Naming a thorn a dependency brought into a base package keeps it as
/// named, with nothing installed again: it stays when that dependency goes.
#[test]
fn naming_a_brought_in_base_thorn_keeps_it() {
    let it = admbase("^1.0");
    let s = &it.setup;
    s.add(&["max/app"], yes()).unwrap();
    assert!(!s.meta().added_for["et/admbase"].requested);
    s.add(&["et/admbase/StaticConformal"], AddFlags::default()).unwrap_or_else(|e| panic!("{e:#}"));
    assert!(s.meta().added_for["et/admbase"].requested);
    s.remove(&["max/app"], remove_yes()).unwrap();
    assert_eq!(s.added(), [("et/admbase".to_owned(), vec!["ADMBase/StaticConformal".to_owned()])]);
    let _ = (&it.a1, &it.a2, &it.log, &it.fake);
}

/// After `inst refetch` moves the base to another commit, the thorns a
/// dependency gave its package follow it, and are still that package: a
/// further dependent of it is no clash, and the entry grows at the new
/// commit.
#[test]
fn a_base_extension_follows_a_refetch_of_the_base() {
    let it = admbase("^1.0");
    let s = &it.setup;
    s.add(&["max/app"], yes()).unwrap();
    let pristine = std::fs::read_to_string(s.inst.source_thornlist()).unwrap();
    let moved = pristine.replace(&it.a1, &it.a2);
    let file = s.inst.root.join("moved.th");
    std::fs::write(&file, &moved).unwrap();
    let args = match <crate::args::Args as clap::Parser>::try_parse_from(["cactup", "inst", "refetch", file.to_str().unwrap()])
        .unwrap()
        .command
    {
        crate::args::Commands::Inst(crate::args::InstallationCommand::Refetch(args)) => args,
        other => panic!("{other:?}"),
    };
    super::super::refetch::dispatch(&s.ctx, args).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(crate::fetch::FetchState::read(&s.inst.root).unwrap().unwrap().repos["admbase"].head, it.a2);

    s.add(&["max/app3"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    let meta = s.meta();
    let admbase = meta.added.iter().find(|a| a.package == "et/admbase").unwrap();
    let mut thorns: Vec<&str> = admbase.thorns.iter().map(|t| t.checkout.as_str()).collect();
    thorns.sort();
    assert_eq!(thorns, ["ADMBase/ADMMacros", "ADMBase/StaticConformal"]);
    assert_eq!(admbase.commit, it.a2);
    assert!(s.inst.root.join("Cactus/arrangements/ADMBase/ADMMacros/interface.ccl").is_file());
    let _ = (&it.log, &it.fake);
}

/// One commit frozen by two versions (two tracks): the present release is
/// the one the lookup names, read lazily or in full alike.
#[test]
fn the_lookup_s_version_picks_the_track() {
    let tmp = tempfile::tempdir().unwrap();
    let (url, c) = upstream(tmp.path(), "two", &[("T/interface.ccl", "IMPLEMENTS: t\n")]);
    let (default, mp) = (version("2.0.0", &c, &url, "Arr", &["T"], "[]"), version("mp:2.0.0", &c, &url, "Arr", &["T"], "[]"));
    let (cactarium, _, fake) =
        Fake::new().package("et/two", vec![default.clone(), mp.clone()]).pin(&url, "et/two", None).serve_shared();
    let s = Setup::new(tmp, &section(&url, &c, &["Arr/T"]), &cactarium);
    let pristine = std::fs::read_to_string(s.inst.source_thornlist()).unwrap();
    let list = thornlist::parse(&pristine).unwrap();
    let client = crate::cactarium::Client::new(&s.ctx.db.read().unwrap());
    for full in [false, true] {
        // Either way the first release found at the commit is the other
        // track's: the one read at the commit (the fake answers the last
        // listed), or the first in the whole list.
        fake.lock().unwrap().packages[0].1 =
            if full { vec![mp.clone(), default.clone()] } else { vec![default.clone(), mp.clone()] };
        let mut reg = super::super::resolution::Registry::new(&client, []);
        let options = super::super::resolution::PresentOptions { full, ..Default::default() };
        let set = super::super::resolution::present(&mut reg, &s.inst.root, &list, &[], None, &options);
        reg.finish();
        let set = set.unwrap_or_else(|e| panic!("{e:#}"));
        let labels: Vec<String> = set.present.iter().map(|p| p.release.label()).collect();
        assert_eq!(labels, ["et/two@2.0.0"], "full: {full}");
    }
}

/// A write that only gives an `added-for` entry more (more thorns at the
/// same commit) edits it: what a newer binary put in it stays.
#[test]
fn an_entry_edited_at_the_same_commit_keeps_unknown_keys() {
    let it = admbase("^1.0");
    let s = &it.setup;
    s.add(&["max/app"], yes()).unwrap();
    let path = s.inst.cactup_dir().join("installation.toml");
    let mut doc: toml::Table = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    doc["added-for"]["et/admbase"].as_table_mut().unwrap().insert("cactup-future".into(), toml::Value::Integer(1));
    std::fs::write(&path, toml::to_string(&doc).unwrap()).unwrap();
    s.add(&["max/app2"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    let doc: toml::Table = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(doc["added-for"]["et/admbase"].get("cactup-future").is_some(), "{doc:?}");
    let _ = (&it.a1, &it.a2, &it.log, &it.fake);
}

/// A base that follows a branch (its admbase section unpinned): more thorns
/// of its package live at the commit the clone is at on disk. They are linked
/// in; the clone is neither fetched nor moved, even with new commits
/// upstream.
#[test]
fn more_thorns_of_an_unpinned_base_clone_are_linked_where_it_is() {
    // Its clone on its branch, at the tip: a2.
    let it = admbase_full("^1.0", false, true, |f| f);
    let s = &it.setup;
    let up = s.inst.root.parent().unwrap().join("up/admbase");
    let a3 = crate::fetch::tests::commit_files(&up, &[("ADMBase/interface.ccl", "IMPLEMENTS: admbase\n"), ("NEW", "3\n")]).to_string();
    s.add(&["max/app3"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    let state = crate::fetch::FetchState::read(&s.inst.root).unwrap().unwrap();
    assert_eq!(state.repos["admbase"].head, it.a2, "not moved to {a3}");
    assert!(s.inst.root.join("Cactus/arrangements/ADMBase/ADMMacros/interface.ccl").is_file());
    let meta = s.meta();
    let admbase = meta.added.iter().find(|a| a.package == "et/admbase").unwrap();
    assert_eq!(admbase.commit, it.a2);
    let _ = (&it.a1, &it.log, &it.fake);
}

/// Two base clones of one package: only the first is the package; thorns
/// added to the second stay present, as the added package they are.
#[test]
fn thorns_added_to_a_second_base_clone_of_a_package_stay_present() {
    let tmp = tempfile::tempdir().unwrap();
    let (u1, c1) = upstream(tmp.path(), "one", &[("A/interface.ccl", "IMPLEMENTS: a\n")]);
    let (u2, c2) = upstream(tmp.path(), "two", &[("B/interface.ccl", "IMPLEMENTS: b\n"), ("C/interface.ccl", "IMPLEMENTS: c\n")]);
    let (cactarium, _) = Fake::new()
        .package("et/x", vec![version("1.0.0", &c1, &u1, "Arr", &["A"], "[]"), version("0.9.0", &c2, &u2, "Arr", &["B", "C"], "[]")])
        .pin(&u1, "et/x", None)
        .pin(&u2, "et/x", None)
        .serve();
    let base = format!("{}{}", section(&u1, &c1, &["Arr/A"]), section(&u2, &c2, &["Arr/B"]));
    let s = Setup::new(tmp, &base, &cactarium);
    let added = AddedPackage {
        package: "et/x".into(),
        version: "0.9.0".into(),
        tag: None,
        commit: c2.clone(),
        url: u2.clone(),
        repo: "two".into(),
        thorns: vec![AddedThorn { checkout: "Arr/C".into(), repo_path: "$2".into() }],
        replaces: Vec::new(),
        contact: None,
        libraries: Vec::new(),
    };
    let pristine = std::fs::read_to_string(s.inst.source_thornlist()).unwrap();
    let list = thornlist::parse(&pristine).unwrap();
    let client = crate::cactarium::Client::new(&s.ctx.db.read().unwrap());
    let mut reg = super::super::resolution::Registry::new(&client, []);
    let options = super::super::resolution::PresentOptions::default();
    let set = super::super::resolution::present(&mut reg, &s.inst.root, &list, &[added], None, &options);
    reg.finish();
    let set = set.unwrap_or_else(|e| panic!("{e:#}"));
    let enabled: Vec<String> = set.present.iter().flat_map(|p| p.enabled.iter().cloned()).collect();
    assert!(enabled.contains(&"C".to_owned()), "{enabled:?}");
}

/// A base at a commit no version froze: naming a thorn of its package reads
/// the package there (one read) and keeps it at its commit, `--no-deps` or
/// not; never an upgrade to the newest release.
#[test]
fn a_named_thorn_of_an_unfrozen_base_package_keeps_its_commit() {
    for no_deps in [true, false] {
        let it = admbase_unfrozen("^1.0");
        let s = &it.setup;
        s.add(&["et/admbase/StaticConformal"], AddFlags { no_deps, ..AddFlags::default() }).unwrap_or_else(|e| panic!("{e:#}"));
        let meta = s.meta();
        let admbase = meta.added.iter().find(|a| a.package == "et/admbase").unwrap();
        assert_eq!((admbase.commit.as_str(), admbase.replaces.len()), (it.a1.as_str(), 0), "no_deps {no_deps}");
        assert_eq!(admbase.thorns.iter().map(|t| t.checkout.as_str()).collect::<Vec<_>>(), ["ADMBase/StaticConformal"]);
        assert_eq!(it.fake.lock().unwrap().unfrozen_reads, 1);
        let _ = (&it.a2, &it.log);
    }
}

/// A dependency on a thorn the unfrozen base package has but does not
/// enable: read there, the thorn added, no false "pass --upgrade".
#[test]
fn a_needed_thorn_of_an_unfrozen_base_package_is_found() {
    let it = admbase_unfrozen("^1.0");
    let s = &it.setup;
    s.add(&["max/app"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    let added = s.added();
    assert!(added.contains(&("et/admbase".to_owned(), vec!["ADMBase/StaticConformal".to_owned()])), "{added:?}");
    let _ = (&it.a1, &it.a2, &it.log);
}

/// The read refused (429: the server's limit on reads at unfrozen commits):
/// a plan that hinges on the package refuses, naming it and `--no-deps`;
/// one that does not touch it goes ahead.
#[test]
fn an_unfrozen_base_package_the_server_will_not_read_refuses_what_hinges_on_it() {
    let it = admbase_unfrozen("^1.0");
    let s = &it.setup;
    it.fake.lock().unwrap().always_limited = true;
    super::super::resolution::TEST_READ_WINDOW.with(|w| w.set(Some(std::time::Duration::from_millis(200))));
    let err = format!("{:#}", s.add(&["max/app"], yes()).unwrap_err());
    assert!(err.contains("et/admbase#") && err.contains("the Cactarium's read limit; try again in a minute"), "{err}");
    assert!(err.contains("--upgrade") && err.contains("--no-deps"), "{err}");
    s.add(&["max/solo"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    let _ = (&it.a1, &it.a2, &it.log);
}

/// A registered fork in the base at a commit no version froze, which shares
/// thorn names with what the plan brings in: read there, it stands in for
/// et/carpet, which max/d needs; et/carpet is not added beside it.
#[test]
fn an_unfrozen_base_fork_stands_in() {
    let tmp = tempfile::tempdir().unwrap();
    let (furl, f) = upstream(
        tmp.path(),
        "carpet-mp",
        &[("Carpet/interface.ccl", "IMPLEMENTS: driver\n"), ("CarpetSlab/interface.ccl", "IMPLEMENTS: hyperslab\n")],
    );
    let (durl, d) = upstream(tmp.path(), "d", &[("D/interface.ccl", "IMPLEMENTS: d\n")]);
    let fork = r#"[{"kind":"fork-of","target":"et/carpet","range":"=2026.5.0","note":null}]"#;
    let claims = format!("[{}]", depends("et/carpet", "^2026.5", &["CarpetSlab"]));
    let mut fake = Fake::new()
        .package("max/carpet-mp", vec![version("1.0.0", &"9".repeat(40), &furl, "Carpet", &["Carpet", "CarpetSlab"], fork)])
        .package("et/carpet", vec![version("2026.5.0", &"e".repeat(40), "https://x/carpet.git", "Carpet", &["Carpet", "CarpetSlab"], "[]")])
        .package("max/d", vec![version("1.0.0", &d, &durl, "D", &["D"], &claims)])
        .pin(&furl, "max/carpet-mp", None);
    let read = version(&f, &f, &furl, "Carpet", &["Carpet", "CarpetSlab"], fork);
    fake.snapshots.push(("max/carpet-mp".to_owned(), f.clone(), read));
    let (cactarium, _) = fake.serve();
    let s = Setup::new(tmp, &section(&furl, &f, &["Carpet/Carpet"]), &cactarium);
    s.add(&["max/d"], yes()).unwrap_or_else(|e| panic!("{e:#}"));
    let added = s.added();
    assert!(added.contains(&("max/carpet-mp".to_owned(), vec!["Carpet/CarpetSlab".to_owned()])), "{added:?}");
    assert!(!added.iter().any(|(p, _)| p == "et/carpet"), "{added:?}");
}

/// The requests on a base of twenty registered repos at commits no version
/// froze, when a dependency names a thorn of one of them that is not
/// enabled: that one package is read at its commit, nothing else.
#[test]
fn a_dependency_on_an_unfrozen_base_package_reads_only_it() {
    let tmp = tempfile::tempdir().unwrap();
    let mut base = String::new();
    let mut fake = Fake::new();
    let mut r0 = (String::new(), String::new());
    for i in 0..20 {
        let name = format!("r{i}");
        let (url, c) = upstream(tmp.path(), &name, &[(&format!("T{i}/interface.ccl"), "IMPLEMENTS: x\n"), (&format!("U{i}/interface.ccl"), "IMPLEMENTS: u\n")]);
        base.push_str(&section(&url, &c, &[&format!("Arr{i}/T{i}")]));
        let id = format!("et/{name}");
        let other = format!("{:040x}", i + 1);
        let thorns = [format!("T{i}"), format!("U{i}")];
        let thorns: Vec<&str> = thorns.iter().map(String::as_str).collect();
        fake = fake.package(&id, vec![version("1.0.0", &other, &url, &format!("Arr{i}"), &thorns, "[]")]);
        fake.snapshots.push((id.clone(), c.clone(), version(&c, &c, &url, &format!("Arr{i}"), &thorns, "[]")));
        fake = fake.pin(&url, &id, None);
        if i == 0 {
            r0 = (url, c);
        }
    }
    let (durl, d) = upstream(tmp.path(), "dep", &[("Dep/interface.ccl", "IMPLEMENTS: dep\n")]);
    let claims = format!("[{}]", depends("et/r0", ">=0.0.0", &["U0"]));
    let (cactarium, log, fake) =
        fake.package("max/dep", vec![version("1.0.0", &d, &durl, "Dep", &["Dep"], &claims)]).serve_shared();
    let s = Setup::new(tmp, &base, &cactarium);
    log.lock().unwrap().clear();
    let got = records(&s, &["max/dep"], &AddFlags::default()).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(record_of(&got, "et/r0").2, ["Arr0/U0"]);
    // [resolve, lookup, whole version lists, one version at a commit, other]
    assert_eq!(counts(&log), [1, 1, 1, 1, 0]);
    assert_eq!(fake.lock().unwrap().unfrozen_reads, 1);
    let _ = r0;
}

/// A base clone moved by hand (a clean `git pull` in a base that follows
/// its branch) is read where its HEAD is, not where the fetch record says:
/// thorns are never linked in from a commit the clone is no longer at.
#[test]
fn a_base_clone_moved_by_hand_is_read_at_its_head() {
    let it = admbase_full("^1.0", false, true, |f| f);
    let s = &it.setup;
    // Upstream moves on, without ADMMacros; the clone is pulled there, and
    // the fetch record left as it was.
    let up = s.inst.root.parent().unwrap().join("up/admbase");
    let a3 = crate::fetch::tests::commit_files(
        &up,
        &[("ADMBase/interface.ccl", "IMPLEMENTS: admbase\n"), ("ADMCoupling/interface.ccl", "IMPLEMENTS: admcoupling\n")],
    )
    .to_string();
    let state = s.inst.root.join(".cactup/fetch-state.toml");
    let recorded = std::fs::read_to_string(&state).unwrap();
    let live = std::fs::read_to_string(s.inst.live_thornlist().unwrap()).unwrap();
    let list = thornlist::parse(&live).unwrap();
    let mut progress = prodash::tree::Root::new().add_child("plan");
    let plan = crate::fetch::plan(&list, &s.inst.root, &mut progress).unwrap();
    let report = crate::fetch::execute(&plan, &s.inst.root).unwrap();
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    std::fs::write(&state, recorded).unwrap();
    assert_eq!(crate::fetch::FetchState::read(&s.inst.root).unwrap().unwrap().repos["admbase"].head, it.a2);
    assert_eq!(super::clone_head(&s.inst.root, "Cactus", "admbase").as_deref(), Some(a3.as_str()));

    // ADMMacros is not at HEAD: no link into it; the add says why.
    let result = s.add(&["max/app3"], yes());
    let macros = s.inst.root.join("Cactus/arrangements/ADMBase/ADMMacros/interface.ccl");
    match result {
        Ok(()) => assert!(macros.is_file(), "a link to a thorn the clone does not have"),
        Err(e) => {
            let e = format!("{e:#}");
            assert!(e.contains(&a3[..12]), "{e}");
            assert!(!macros.exists());
        }
    }
    let _ = (&it.a1, &it.log, &it.fake);
}
