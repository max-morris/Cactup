//! `cactup package init`, end to end: real upstream repos on disk, an
//! installation fetched from them and built (a config record and its `ThornList`), and a fake
//! Cactarium (`test_server`) answering `/lookup`, version lists, reads at a commit, `/whoami`,
//! `/repos` and a refresh in the crate's shapes.
// Plan §8 (integration tests), the P7 row of §7's reviewer checks.

use super::*;
use crate::installation::{AddedPackage, AddedThorn};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

/// Every request the fake Cactarium got: its path and body.
type Log = Arc<Mutex<Vec<(String, String)>>>;

/// One `PackageVersion`: its thorns (arrangement, thorn) and claims.
fn version(v: &str, commit: &str, url: &str, thorns: &[(&str, &str)], claims: Value) -> Value {
    let thorns: Vec<Value> = (thorns.iter())
        .map(|(a, t)| json!({"arrangement": a, "thorn": t, "repo-path": "$2", "facts": ThornFacts::default()}))
        .collect();
    json!({"version": v, "track": null, "tag": format!("v{v}"), "commit": commit, "tree": null, "yanked": false,
        "yank-reason": null, "state": "ok", "curated": false, "published": 0, "url": url, "thorns": thorns,
        "claims": claims})
}

/// The fake Cactarium.
#[derive(Clone, Default)]
struct Fake {
    /// Package id → its versions.
    packages: Vec<(String, Vec<Value>)>,
    /// URL → its package, the package's `fork-of` there, and whether it is a flesh.
    pins: Vec<(String, String, Vec<String>, bool)>,
    /// The package is on Cactarium already: `/repos` answers `existing`.
    registered: bool,
    /// `owner/name` → its release 1.0.0, frozen.
    stacks: Vec<(String, Value)>,
}

impl Fake {
    fn package(mut self, id: &str, versions: Vec<Value>) -> Fake {
        self.packages.push((id.to_owned(), versions));
        self
    }

    fn pin(mut self, url: &str, package: &str) -> Fake {
        self.pins.push((url.to_owned(), package.to_owned(), Vec::new(), false));
        self
    }

    fn serve(self) -> (String, Log) {
        let log: Log = Arc::default();
        let seen = log.clone();
        let (base, _) = crate::fetch::download::test_server::serve_with_body(1000, move |path, body| {
            seen.lock().unwrap().push((path.to_owned(), String::from_utf8_lossy(body).into_owned()));
            self.answer(path, body)
        });
        crate::cactarium::TEST_TOKENS.lock().unwrap().insert(base.clone(), "token".into());
        (base, log)
    }

    fn answer(&self, path: &str, body: &[u8]) -> (&'static str, String, Vec<u8>) {
        let ok = |v: Value| ("200 OK", "api-generation: 1\r\n".to_owned(), v.to_string().into_bytes());
        let missing = || {
            let body = br#"{"error":{"code":"not-found","message":"Not found"}}"#.to_vec();
            ("404 Not Found", "api-generation: 1\r\n".to_owned(), body)
        };
        let path = path.strip_prefix("/api/v1").unwrap_or(path);
        let frozen = |id: &str, commit: &str| {
            let (_, versions) = self.packages.iter().find(|(p, _)| p == id)?;
            versions.iter().find(|v| v["commit"] == commit).cloned()
        };
        match path {
            "/whoami" => return ok(json!({"user": "max", "orgs": ["et"], "admin": false})),
            "/lookup" => {
                let asked: cactup_ecosystem_common::api::LookupRequest = serde_json::from_slice(body).unwrap();
                let pins: Vec<Value> = (asked.pins.iter())
                    .map(|p| match self.pins.iter().find(|(u, ..)| *u == p.url) {
                        Some((_, package, fork_of, flesh)) => {
                            let version = frozen(package, &p.commit).map(|v| v["version"].clone());
                            json!({"url": p.url, "commit": p.commit, "package": package, "version": version,
                                "flesh": flesh, "fork-of": fork_of, "commit-known": null})
                        }
                        None => json!({"url": p.url, "commit": p.commit, "package": null, "version": null,
                            "flesh": false, "fork-of": [], "commit-known": null}),
                    })
                    .collect();
                return ok(json!({ "pins": pins }));
            }
            "/repos" => {
                let asked: RegisterRequest = serde_json::from_slice(body).unwrap();
                let name = asked.only.first().cloned().unwrap_or_default();
                let mut entry = json!({"slug": format!("max/{name}"), "kind": "package", "file": "cactarium.toml",
                    "tag": "v1.0.0", "version": "1.0.0", "rule": "tags vX.Y.Z", "review": "pending",
                    "status": if self.registered { "existing" } else { "new" }});
                if !self.registered && !asked.dry_run {
                    assert_eq!(asked.expect.as_deref(), Some("fingerprint"), "registers what the preview showed");
                    entry["url"] = json!(format!("https://cactarium.test/max/{name}"));
                    entry["new-versions"] = json!(1);
                    entry["awaiting-review"] = json!(1);
                }
                let expect = asked.dry_run.then_some("fingerprint");
                return ok(json!({"entries": [entry], "warnings": [], "expect": expect}));
            }
            _ => {}
        }
        if let Some(id) = path.strip_prefix("/stacks/").and_then(|r| r.strip_suffix("/releases")) {
            return ok(json!({"slug": format!("{id}@1.0.0"), "url": "u", "visibility": "public", "warnings": []}));
        }
        if let Some(rest) = path.strip_prefix("/stacks/") {
            let id = rest.strip_suffix("/releases/1.0.0").unwrap_or(rest);
            return match self.stacks.iter().find(|(s, _)| s == id) {
                Some((_, frozen)) => ok(frozen.clone()),
                None => missing(),
            };
        }
        if let Some(id) = path.strip_prefix("/packages/").and_then(|r| r.strip_suffix("/refresh")) {
            return ok(json!({"slug": id, "new-versions": 2, "awaiting-review": 0}));
        }
        if let Some(rest) = path.strip_prefix("/packages/") {
            let Some((id, at)) = rest.split_once("/versions") else { return missing() };
            let Some((_, versions)) = self.packages.iter().find(|(p, _)| p == id) else { return missing() };
            return match at.trim_start_matches('/') {
                "" => ok(json!({"slug": id, "versions": versions})),
                commit => match frozen(id, commit) {
                    Some(v) => ok(v),
                    None => missing(),
                },
            };
        }
        missing()
    }
}

/// An upstream repo `name` with `files` (the whole tree of the new commit), as a local URL and
/// its commit.
fn upstream(tmp: &Path, name: &str, files: &[(&str, &str)]) -> (String, String) {
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

/// An installation (alias `t`, active, on `stack` when given) of `base`, fetched, with
/// Cactarium at `cactarium`.
struct World {
    _tmp: tempfile::TempDir,
    ctx: Ctx,
    inst: Installation,
}

impl World {
    fn new(tmp: tempfile::TempDir, base: &str, cactarium: &str, stack: Option<&str>) -> World {
        let root = tmp.path().join("inst");
        let db = crate::database::Db::in_dir(&tmp.path().join("db"));
        let path = root.display().to_string();
        let (cactarium, stack) = (cactarium.to_owned(), stack.map(str::to_owned));
        db.update(move |d| {
            d.installations.insert(
                "t".into(),
                crate::database::CactusInstallation {
                    alias: "t".into(),
                    release: None,
                    path,
                    thornlist: None,
                    simfile: None,
                    stack,
                    current_release: None,
                    current_thornlist: None,
                    stack_contact: None,
                    unfetched_repos: Default::default(),
                    copied_from: None,
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
        World { _tmp: tmp, ctx, inst }
    }

    fn cactus(&self) -> PathBuf {
        self.inst.root.join("Cactus")
    }

    fn repo(&self, name: &str) -> PathBuf {
        self.cactus().join("repos").join(name)
    }

    /// Config `sim`, active, built from `thorns` (its `ThornList`), with the source state of
    /// each repo of `sources`.
    fn built(&self, thorns: &[&str], sources: &[(&str, &str)]) {
        let dir = self.cactus().join("configs/sim");
        std::fs::create_dir_all(&dir).unwrap();
        let list: String = thorns.iter().map(|t| format!("{t}\n")).collect();
        std::fs::write(dir.join("ThornList"), format!("# the thorns\n{list}")).unwrap();
        let mut config =
            crate::build::ConfigMeta::exhaustive_sample(crate::build::OptionlistSource::Flavor("generic".into()));
        config.sources = (!sources.is_empty()).then(|| sources.iter().map(|(r, c)| ((*r).to_owned(), (*c).to_owned())).collect());
        crate::format::replace(&crate::build::ConfigMeta::path_for(&self.cactus(), "sim"), &config, &crate::build::CONFIG)
            .unwrap();
        self.edit_meta(|m| m.active_config = Some("sim".into()));
    }

    fn edit_meta(&self, edit: impl FnOnce(&mut InstallationMeta)) {
        let locked = self.inst.locked().unwrap();
        let mut meta = locked.meta().unwrap();
        edit(&mut meta);
        locked.set_meta(&meta).unwrap();
    }

    fn init(&self, repo: &str, exact: bool, force: bool) -> Res<()> {
        let args = PackageInitArgs { repo: Some(repo.into()), name: None, owner: None, exact, force, publish: false };
        init(&self.ctx, &args)
    }

    fn publish(&self, repo: &str) -> Res<()> {
        let args = PackageInitArgs { repo: Some(repo.into()), name: None, owner: None, exact: false, force: false, publish: true };
        init(&self.ctx, &args)
    }

    /// The manifest written for `repo`, checked to parse.
    fn manifest(&self, repo: &str) -> (String, Manifest) {
        let text = std::fs::read_to_string(self.repo(repo).join(PACKAGE_FILE)).unwrap();
        let parsed = Manifest::from_repo_file(PACKAGE_FILE, &text).unwrap_or_else(|e| panic!("{e}\n{text}"));
        (text, parsed)
    }
}

/// The text's lines that are no comment.
fn claims_of(text: &str) -> Vec<&str> {
    text.lines().filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty()).collect()
}

/// et/carpetx (CarpetX, and Loop, which implements loop and provides loop.hxx) at 1.0.0 then
/// 1.1.0, installed at 1.1.0; the repo z4c (Z4c, inheriting loop and including a file of Loop's
/// by path, and Extra, which no config builds) is no registered package.
struct Z4c {
    world: World,
    log: Log,
    c1: String,
    c2: String,
}

fn z4c() -> Z4c {
    let tmp = tempfile::tempdir().unwrap();
    let carpetx = [
        ("CarpetX/interface.ccl", "IMPLEMENTS: carpetx\n"),
        ("Loop/interface.ccl", "IMPLEMENTS: loop\nINCLUDE HEADER: loop.hxx IN loop.hxx\n"),
    ];
    let (curl, c1) = upstream(tmp.path(), "carpetx", &carpetx);
    let mut later = carpetx.to_vec();
    later.push(("README", "1.1\n"));
    let (_, c2) = upstream(tmp.path(), "carpetx", &later);
    let (zurl, z) = upstream(
        tmp.path(),
        "z4c",
        &[
            ("Z4c/interface.ccl", "IMPLEMENTS: z4c\nINHERITS: loop\n"),
            ("Z4c/src/rhs.cc", "#include \"cctk.h\"\n#include \"../../../CarpetX/Loop/src/loop.hxx\"\n"),
            ("Extra/interface.ccl", "IMPLEMENTS: extra\n"),
        ],
    );
    let (cactarium, log) = Fake::default()
        .package(
            "et/carpetx",
            vec![
                version("1.1.0", &c2, &curl, &[("CarpetX", "CarpetX"), ("CarpetX", "Loop")], json!([])),
                version("1.0.0", &c1, &curl, &[("CarpetX", "CarpetX"), ("CarpetX", "Loop")], json!([])),
            ],
        )
        .pin(&curl, "et/carpetx")
        .serve();
    let base = section(&curl, &c2, &["CarpetX/CarpetX", "CarpetX/Loop"]) + &section(&zurl, &z, &["Z4c/Z4c"]);
    let world = World::new(tmp, &base, &cactarium, None);
    world.built(&["CarpetX/CarpetX", "CarpetX/Loop", "Z4c/Z4c"], &[]);
    Z4c { world, log, c1, c2 }
}

/// A requirement met by one built thorn of a registered package becomes a dependency on it
/// with `needed-by` explicit, floored at the version the config was built with (1.0.0 here,
/// though the clone is at 1.1.0 now); `--exact` writes that version exactly; an existing file
/// is refused without `-f`. The owner is the token's user, and the output parses as a manifest.
#[test]
fn a_provider_becomes_a_dependency_floored_at_the_build() {
    let it = z4c();
    let c1 = it.c1.clone();
    let w = &it.world;
    w.built(&["CarpetX/CarpetX", "CarpetX/Loop", "Z4c/Z4c"], &[("carpetx", c1.as_str())]);
    w.init("z4c", false, false).unwrap_or_else(|e| panic!("{e:#}"));
    let (text, m) = w.manifest("z4c");
    let p = m.package.as_ref().unwrap();
    assert_eq!((p.name.as_str(), p.owners[0].as_str()), ("z4c", "max"), "{text}");
    assert_eq!(p.arrangement.as_deref(), Some("Z4c"), "the folders do not name the arrangement: {text}");
    assert_eq!(m.dependencies.len(), 1, "{text}");
    let d = &m.dependencies[0];
    assert_eq!(d.target.as_ref().unwrap().to_string(), "et/carpetx@~1.0.0", "{text}");
    assert_eq!(d.needs.as_deref(), Some(&["Loop".to_owned()][..]));
    assert_eq!(d.needed_by, ["Z4c"]);
    assert!(m.fork_of.is_empty() && m.works_with.is_empty() && m.incompatible_with.is_empty(), "{text}");

    let err = format!("{:#}", w.init("z4c", true, false).unwrap_err());
    assert!(err.contains("exists") && err.contains("Pass -f"), "{err}");
    w.init("z4c", true, true).unwrap();
    let (text, m) = w.manifest("z4c");
    assert_eq!(m.dependencies[0].target.as_ref().unwrap().to_string(), "et/carpetx@=1.0.0", "{text}");
    assert!(it.log.lock().unwrap().iter().any(|(p, _)| p == "/api/v1/whoami"));
}

/// Without a record of the build's sources, the floor is where the clone is (1.1.0), not the
/// thornlist's pin (1.0.0 here).
#[test]
fn without_build_sources_the_floor_is_the_clone() {
    let it = z4c();
    let pristine = std::fs::read_to_string(it.world.inst.source_thornlist()).unwrap();
    let pinned = pristine.replace(&format!("!REPO_COMMIT = {}", it.c2), &format!("!REPO_COMMIT = {}", it.c1));
    assert_ne!(pinned, pristine);
    std::fs::write(it.world.inst.source_thornlist(), pinned).unwrap();
    it.world.init("z4c", false, false).unwrap();
    let (text, m) = it.world.manifest("z4c");
    assert_eq!(m.dependencies[0].target.as_ref().unwrap().to_string(), "et/carpetx@~1.1.0", "{text}");
}

/// Smells are TODO comments, never claims: a thorn the config does not build (a possible
/// exclude-thorns), and an `#include` that climbs into another installed thorn.
#[test]
fn smells_are_todos() {
    let it = z4c();
    it.world.init("z4c", false, false).unwrap();
    let (text, m) = it.world.manifest("z4c");
    assert!(text.contains("# TODO: not built in config sim: Extra.\n"), "{text}");
    assert!(text.contains("# exclude-thorns = [\"Extra\"]\n"), "{text}");
    assert!(m.package.as_ref().unwrap().exclude_thorns.is_empty());
    assert!(
        text.contains("# TODO: Z4c includes \"../../../CarpetX/Loop/src/loop.hxx\" from CarpetX/Loop (et/carpetx).\n"),
        "{text}"
    );
    // The clone cactup fetched is shallow: nothing to tell a fork by, and nothing to fork here.
    assert!(!text.contains("[[fork-of]]"), "{text}");
}

/// A provider in a repo that is no registered package, or at a commit no version froze, is a
/// TODO, never a dependency.
#[test]
fn an_unregistered_or_unfrozen_provider_is_a_todo() {
    let tmp = tempfile::tempdir().unwrap();
    let (lurl, l) = upstream(tmp.path(), "loops", &[("Loop/interface.ccl", "IMPLEMENTS: loop\n")]);
    let (aurl, a) = upstream(tmp.path(), "arith", &[("Arith/interface.ccl", "IMPLEMENTS: arith\n")]);
    let (zurl, z) = upstream(tmp.path(), "z4c", &[("Z4c/interface.ccl", "IMPLEMENTS: z4c\nINHERITS: loop arith\n")]);
    let (cactarium, _) = Fake::default()
        .package("et/arith", vec![version("1.0.0", &"0".repeat(40), &aurl, &[("Arith", "Arith")], json!([]))])
        .pin(&aurl, "et/arith")
        .serve();
    let base = section(&lurl, &l, &["Loop/Loop"]) + &section(&aurl, &a, &["Arith/Arith"]) + &section(&zurl, &z, &["Z4c/Z4c"]);
    let w = World::new(tmp, &base, &cactarium, None);
    w.built(&["Loop/Loop", "Arith/Arith", "Z4c/Z4c"], &[]);
    w.init("z4c", false, false).unwrap();
    let (text, m) = w.manifest("z4c");
    assert!(m.dependencies.is_empty(), "{text}");
    assert!(
        text.contains("# TODO: implementation loop, for Z4c: provided by Loop/Loop from repo loops, which is no registered package.\n"),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "# TODO: implementation arith, for Z4c: provided by Arith/Arith of et/arith at commit {}, which no version froze.\n",
            &a[..12]
        )),
        "{text}"
    );
}

/// An implementation two built packages provide (Carpet and PUGH both implement driver) is a
/// TODO any-of, written as comments only; and a thorn inheriting the generic driver while
/// including Carpet's header is a smell.
#[test]
fn several_implementations_are_a_todo_any_of() {
    let tmp = tempfile::tempdir().unwrap();
    let (curl, c) = upstream(
        tmp.path(),
        "carpet",
        &[("Carpet/interface.ccl", "IMPLEMENTS: driver\nINCLUDE HEADER: carpet.hh IN carpet.hh\n")],
    );
    let (purl, p) = upstream(tmp.path(), "pugh", &[("PUGH/interface.ccl", "IMPLEMENTS: driver\n")]);
    let (zurl, z) = upstream(
        tmp.path(),
        "z4c",
        &[("Z4c/interface.ccl", "IMPLEMENTS: z4c\nINHERITS: driver\n"), ("Z4c/src/a.cc", "#include <carpet.hh>\n")],
    );
    let (cactarium, _) = Fake::default()
        .package("et/carpet", vec![version("2.0.0", &c, &curl, &[("Carpet", "Carpet")], json!([]))])
        .package("et/pugh", vec![version("3.1.0", &p, &purl, &[("CactusPUGH", "PUGH")], json!([]))])
        .pin(&curl, "et/carpet")
        .pin(&purl, "et/pugh")
        .serve();
    let base = section(&curl, &c, &["Carpet/Carpet"]) + &section(&purl, &p, &["CactusPUGH/PUGH"]) + &section(&zurl, &z, &["Z4c/Z4c"]);
    let w = World::new(tmp, &base, &cactarium, None);
    w.built(&["Carpet/Carpet", "CactusPUGH/PUGH", "Z4c/Z4c"], &[]);
    w.init("z4c", false, false).unwrap();
    let (text, m) = w.manifest("z4c");
    assert!(m.dependencies.is_empty(), "no claim for an ambiguous provider: {text}");
    let any_of = "# TODO: implementation driver, for Z4c: several installed packages provide it.\n\
                  # Depend on the one these thorns need, or on any of them:\n\
                  # [[dependency]]\n# any-of = [\n\
                  #     { target = \"et/carpet@~2.0.0\", needs = [\"Carpet\"] },\n\
                  #     { target = \"et/pugh@~3.1.0\", needs = [\"PUGH\"] },\n\
                  # ]\n# needed-by = [\"Z4c\"]\n";
    assert!(text.contains(any_of), "{text}");
    assert!(
        text.contains("# TODO: Z4c inherits the generic driver, but includes carpet.hh, provided by Carpet/Carpet (et/carpet).\n"),
        "{text}"
    );
    assert!(!text.contains("# TODO: Z4c inherits the generic driver, but includes carpet.hh, provided by CactusPUGH"), "{text}");
}

/// A requirement only an unbuilt thorn meets is met by it; nothing meeting it at all is a TODO.
#[test]
fn an_unmet_requirement_is_a_todo() {
    let tmp = tempfile::tempdir().unwrap();
    let (zurl, z) = upstream(tmp.path(), "z4c", &[("Z4c/interface.ccl", "IMPLEMENTS: z4c\nINHERITS: nowhere\n")]);
    let (cactarium, _) = Fake::default().serve();
    let w = World::new(tmp, &section(&zurl, &z, &["Z4c/Z4c"]), &cactarium, None);
    w.built(&["Z4c/Z4c"], &[]);
    w.init("z4c", false, false).unwrap();
    let (text, _) = w.manifest("z4c");
    assert!(text.contains("# TODO: implementation nowhere, for Z4c: no installed thorn provides it.\n"), "{text}");
}

/// Copies a directory tree (a clone with its whole history).
fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let (src, dst) = (e.path(), to.join(e.file_name()));
        if e.file_type().unwrap().is_dir() {
            copy_tree(&src, &dst);
        } else {
            std::fs::copy(&src, &dst).unwrap();
        }
    }
}

/// et/carpet (Carpet, CarpetLib) at 1.0.0 then 1.1.0 in the base; the added max/carpet-mp
/// replaces it, its clone holding `history` (a fork of carpet, or not) with `files`.
fn carpet_fork(fork: bool, files: &[(&str, &str)]) -> (World, String) {
    let tmp = tempfile::tempdir().unwrap();
    let carpet = [("Carpet/interface.ccl", "IMPLEMENTS: driver\n"), ("CarpetLib/interface.ccl", "IMPLEMENTS: carpetlib\n")];
    let (curl, c1) = upstream(tmp.path(), "carpet", &carpet);
    let mut later = carpet.to_vec();
    later.push(("NEWS", "1.1\n"));
    let (_, c2) = upstream(tmp.path(), "carpet", &later);
    let mp = tmp.path().join("up/carpet-mp");
    if fork {
        // Forked at 1.0.0: 1.1.0 is not in its history.
        copy_tree(&tmp.path().join("up/carpet"), &mp);
        std::fs::write(mp.join(".git/HEAD"), format!("{c1}\n")).unwrap();
    } else {
        crate::fetch::git::testrepo::init(&mp);
    }
    let m = crate::fetch::tests::commit_files(&mp, files).to_string();
    // The worktree as the commit has it: no thorn left behind by the copy.
    for t in ["Carpet", "CarpetLib"] {
        if !files.iter().any(|(f, _)| f.starts_with(&format!("{t}/"))) {
            std::fs::remove_dir_all(mp.join(t)).unwrap();
        }
    }
    let (cactarium, _) = Fake::default()
        .package(
            "et/carpet",
            vec![
                version("1.1.0", &c2, &curl, &[("Carpet", "Carpet"), ("Carpet", "CarpetLib")], json!([])),
                version("1.0.0", &c1, &curl, &[("Carpet", "Carpet"), ("Carpet", "CarpetLib")], json!([])),
            ],
        )
        .pin(&curl, "et/carpet")
        .serve();
    let w = World::new(tmp, &section(&curl, &c2, &["Carpet/Carpet", "Carpet/CarpetLib"]), &cactarium, None);
    let url = mp.display().to_string();
    w.edit_meta(|meta| {
        meta.added.push(AddedPackage {
            package: "max/carpet-mp".into(),
            version: m.clone(),
            tag: None,
            commit: m.clone(),
            url: url.clone(),
            repo: "carpet-mp".into(),
            thorns: vec![AddedThorn { checkout: "Carpet/Carpet".into(), repo_path: "$2".into() }],
            replaces: vec!["carpet".into()],
            contact: None,
            libraries: Vec::new(),
            flesh: Vec::new(),
        })
    });
    // Its clone, with the whole history.
    copy_tree(&mp, &w.repo("carpet-mp"));
    w.built(&["Carpet/Carpet"], &[]);
    (w, c1)
}

/// A repo whose history contains a version's frozen commit and which has every thorn of it is a
/// fork of that version, exactly (not of 1.1.0, which it does not contain).
#[test]
fn a_fork_is_detected_by_history_and_thorns() {
    let files = [
        ("Carpet/interface.ccl", "IMPLEMENTS: driver\n# mixed precision\n"),
        ("CarpetLib/interface.ccl", "IMPLEMENTS: carpetlib\n"),
    ];
    let (w, _) = carpet_fork(true, &files);
    w.init("carpet-mp", false, false).unwrap_or_else(|e| panic!("{e:#}"));
    let (text, m) = w.manifest("carpet-mp");
    assert_eq!(m.fork_of.len(), 1, "{text}");
    assert_eq!(m.fork_of[0].target.to_string(), "et/carpet@=1.0.0", "{text}");
}

/// A fork lacking some of the upstream version's thorns is no drop-in: the fork-of is a TODO.
#[test]
fn a_fork_missing_thorns_is_a_todo() {
    let (w, _) = carpet_fork(true, &[("Carpet/interface.ccl", "IMPLEMENTS: driver\n# mp\n")]);
    w.init("carpet-mp", false, false).unwrap();
    let (text, m) = w.manifest("carpet-mp");
    assert!(m.fork_of.is_empty(), "{text}");
    assert!(text.contains("# TODO: this repo contains et/carpet@=1.0.0 but lacks its thorns CarpetLib.\n"), "{text}");
    assert!(text.contains("# [[fork-of]]\n# target = \"et/carpet@=1.0.0\"\n"), "{text}");
}

/// Thorns of the same names with an unrelated history are no fork.
#[test]
fn shared_names_without_shared_history_are_no_fork() {
    // Not carpet's own first commit either (same tree, same signature: the same commit).
    let files = [("Carpet/interface.ccl", "IMPLEMENTS: driver\n# new\n"), ("CarpetLib/interface.ccl", "IMPLEMENTS: carpetlib\n")];
    let (w, _) = carpet_fork(false, &files);
    w.init("carpet-mp", false, false).unwrap();
    let (text, m) = w.manifest("carpet-mp");
    assert!(m.fork_of.is_empty() && !text.contains("fork-of"), "{text}");
}

/// A shallow clone (as cactup fetches) cannot tell a fork: said as a TODO.
#[test]
fn a_shallow_clone_says_it_cannot_tell_a_fork() {
    let tmp = tempfile::tempdir().unwrap();
    let carpet = [("Carpet/interface.ccl", "IMPLEMENTS: driver\n")];
    let (curl, c) = upstream(tmp.path(), "carpet", &carpet);
    let (murl, m) = upstream(tmp.path(), "carpetmp", &[("Carpet/interface.ccl", "IMPLEMENTS: driver\n# mp\n")]);
    let (cactarium, _) = Fake::default()
        .package("et/carpet", vec![version("1.0.0", &c, &curl, &[("Carpet", "Carpet")], json!([]))])
        .pin(&curl, "et/carpet")
        .serve();
    // Fetched by cactup, depth 1, then replacing carpet.
    let w = World::new(tmp, &(section(&curl, &c, &["Carpet/Carpet"]) + &section(&murl, &m, &["MP/Carpet"])), &cactarium, None);
    assert!(gix::open(w.repo("carpetmp")).unwrap().is_shallow(), "cactup's clones are shallow");
    w.built(&["Carpet/Carpet"], &[]);
    w.init("carpetmp", false, false).unwrap();
    let (text, _) = w.manifest("carpetmp");
    assert!(text.contains("# TODO: the clone of repo carpetmp is shallow, so its history cannot tell whether it forks et/carpet.\n"), "{text}");
}

/// A flesh fork in use is a TODO naming it, with the dependency commented out.
#[test]
fn a_flesh_fork_in_use_is_a_todo() {
    let tmp = tempfile::tempdir().unwrap();
    let (furl, f) = upstream(tmp.path(), "flesh", &[("Makefile", "mp\n"), ("lib/x", "x\n"), ("src/main.c", "mp\n")]);
    let (zurl, z) = upstream(tmp.path(), "z4c", &[("Z4c/interface.ccl", "IMPLEMENTS: z4c\n")]);
    let mut fake = Fake::default().package("max/cactus-mp", vec![version("1.2.0", &f, &furl, &[], json!([]))]);
    fake.pins.push((furl.clone(), "max/cactus-mp".into(), vec!["et/cactus@=4.0.0".into()], true));
    let (cactarium, _) = fake.serve();
    let base = format!(
        "\n!TARGET = $ROOT\n!TYPE = git\n!URL = {furl}\n!NAME = flesh\n!REPO_COMMIT = {f}\n!CHECKOUT = Makefile lib src\n{}",
        section(&zurl, &z, &["Z4c/Z4c"])
    );
    let w = World::new(tmp, &base, &cactarium, None);
    w.built(&["Z4c/Z4c"], &[]);
    w.init("z4c", false, false).unwrap();
    let (text, m) = w.manifest("z4c");
    assert!(m.dependencies.is_empty(), "{text}");
    assert!(
        text.contains(
            "# TODO: the flesh here is max/cactus-mp@1.2.0, a fork of et/cactus@=4.0.0.\n\
             # If these thorns need it, depend on it:\n# [[dependency]]\n# target = \"max/cactus-mp@~1.2.0\"\n"
        ),
        "{text}"
    );
}

/// The installation's claim overrides go in as comments, not claims.
#[test]
fn overrides_go_in_as_comments() {
    let it = z4c();
    it.world.edit_meta(|m| {
        m.claim_overrides.insert(
            "satisfies local:carpetx et/carpetx".into(),
            crate::installation::ClaimOverride { kind: "satisfies".into(), note: Some("our fork".into()) },
        );
    });
    it.world.init("z4c", false, false).unwrap();
    let (text, _) = it.world.manifest("z4c");
    assert!(text.contains("# Claim overrides of installation t:\n#   satisfies local:carpetx et/carpetx  (our fork)\n"), "{text}");
    assert!(claims_of(&text).iter().all(|l| !l.contains("satisfies")), "{text}");
}

/// Marks HEAD of the clone at `dir` as on its remote (as a push and fetch would).
fn mark_pushed(dir: &Path) {
    let mut repo = gix::open(dir).unwrap();
    // CI runners have no git identity, and a ref's log needs one.
    let _ = repo.committer_or_set_generic_fallback();
    let head = repo.head_id().unwrap().detach();
    repo.reference("refs/remotes/origin/main", head, gix::refs::transaction::PreviousValue::Any, "test").unwrap();
}

/// `--publish` writes nothing: it refuses an uncommitted manifest, one with uncommitted changes
/// and an unpushed one, each naming the git commands; then registers it (a preview, then exactly
/// what it showed, at the https URL anyone can fetch), or, when it is registered, refreshes it.
#[test]
fn publish_refuses_until_committed_and_pushed_then_registers_or_refreshes() {
    use crate::fetch::anonymous::testserver::{serve, Mode};
    serve("pkg.share.test", Mode::Public);
    for registered in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let files = [("Z4c/interface.ccl", "IMPLEMENTS: z4c\n")];
        let (zurl, z) = upstream(tmp.path(), "z4c", &files);
        let (cactarium, log) = Fake { registered, ..Fake::default() }.serve();
        let w = World::new(tmp, &section(&zurl, &z, &["Z4c/Z4c"]), &cactarium, None);
        // Published under its https URL from here on.
        let pristine = std::fs::read_to_string(w.inst.source_thornlist()).unwrap();
        std::fs::write(w.inst.source_thornlist(), pristine.replace(&zurl, "https://pkg.share.test/max/z4c.git")).unwrap();
        let dir = w.repo("z4c");
        let err = format!("{:#}", w.publish("z4c").unwrap_err());
        assert!(err.contains("has no cactarium.toml") && err.contains("`cactup package init z4c`"), "{err}");

        let manifest = "format = 1\n[package]\nname = \"z4c\"\nowners = [\"max\"]\n";
        std::fs::write(dir.join(PACKAGE_FILE), manifest).unwrap();
        let err = format!("{:#}", w.publish("z4c").unwrap_err());
        assert!(err.contains("cactarium.toml is not committed in repo z4c"), "{err}");
        assert!(err.contains(&format!("git -C {} add cactarium.toml", dir.display())), "{err}");
        assert!(err.contains(&format!("git -C {} push", dir.display())), "{err}");

        let mut tree = files.to_vec();
        tree.push((PACKAGE_FILE, manifest));
        crate::fetch::tests::commit_files(&dir, &tree);
        std::fs::write(dir.join(PACKAGE_FILE), format!("{manifest}# edited\n")).unwrap();
        let err = format!("{:#}", w.publish("z4c").unwrap_err());
        assert!(err.contains("has uncommitted changes") && err.contains("commit -m"), "{err}");
        std::fs::write(dir.join(PACKAGE_FILE), manifest).unwrap();

        let err = format!("{:#}", w.publish("z4c").unwrap_err());
        assert!(err.contains("is on no remote branch") && err.contains(&format!("git -C {} push", dir.display())), "{err}");
        assert!(!log.lock().unwrap().iter().any(|(p, _)| p.contains("/repos")), "nothing asked before it is pushed");

        mark_pushed(&dir);
        w.publish("z4c").unwrap_or_else(|e| panic!("{e:#}"));
        let asked: Vec<(String, String)> = log.lock().unwrap().clone();
        let repos: Vec<&(String, String)> = asked.iter().filter(|(p, _)| p == "/api/v1/repos").collect();
        let refreshes = asked.iter().filter(|(p, _)| p == "/api/v1/packages/max/z4c/refresh").count();
        assert!(repos[0].1.contains("\"url\":\"https://pkg.share.test/max/z4c.git\""), "{}", repos[0].1);
        assert!(repos[0].1.contains("\"only\":[\"z4c\"]") && repos[0].1.contains("\"dry-run\":true"), "{}", repos[0].1);
        if registered {
            assert_eq!((repos.len(), refreshes), (1, 1), "a registered package is refreshed");
        } else {
            assert_eq!((repos.len(), refreshes), (2, 0), "a preview, then the registration");
            assert!(repos[1].1.contains("\"expect\":\"fingerprint\"") && !repos[1].1.contains("dry-run"), "{}", repos[1].1);
        }
        assert_eq!(std::fs::read_to_string(dir.join(PACKAGE_FILE)).unwrap(), manifest, "--publish writes nothing");
    }
}

/// A remote nobody else could fetch (a local path) is never registered.
#[test]
fn publish_refuses_a_local_remote() {
    let tmp = tempfile::tempdir().unwrap();
    let files = [("Z4c/interface.ccl", "IMPLEMENTS: z4c\n")];
    let (zurl, z) = upstream(tmp.path(), "z4c", &files);
    let (cactarium, log) = Fake::default().serve();
    let w = World::new(tmp, &section(&zurl, &z, &["Z4c/Z4c"]), &cactarium, None);
    let dir = w.repo("z4c");
    let manifest = "format = 1\n[package]\nname = \"z4c\"\nowners = [\"max\"]\n";
    let mut tree = files.to_vec();
    tree.push((PACKAGE_FILE, manifest));
    crate::fetch::tests::commit_files(&dir, &tree);
    mark_pushed(&dir);
    assert!(w.publish("z4c").is_err());
    assert!(!log.lock().unwrap().iter().any(|(p, _)| p.contains("/repos")));
}

/// Ctrl-C while the repo's sources are read fails the command at once and writes nothing. The
/// interrupt flag is the process's: run in a child of its own.
#[test]
fn an_interrupt_during_the_source_scan_writes_nothing() {
    if std::env::var_os("CACTUP_TEST_SCAN_INTERRUPT").is_none() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["commands::package::tests::an_interrupt_during_the_source_scan_writes_nothing", "--exact", "--test-threads=1"])
            .env("CACTUP_TEST_SCAN_INTERRUPT", "1")
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success() && said.contains("1 passed"), "{said}{}", String::from_utf8_lossy(&out.stderr));
        return;
    }
    static READ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let it = z4c();
    // Many sources: the scan stops after the first, not after all of them.
    let src = it.world.repo("z4c").join("Z4c/src");
    for i in 0..300 {
        std::fs::write(src.join(format!("f{i:03}.cc")), "#include \"cctk.h\"\n").unwrap();
    }
    AFTER_SOURCE
        .set(|| {
            READ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            gix::interrupt::IS_INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
        })
        .unwrap();
    let started = std::time::Instant::now();
    let err = format!("{:#}", it.world.init("z4c", false, false).unwrap_err());
    assert!(err.contains("Interrupted"), "{err}");
    assert!(started.elapsed() < std::time::Duration::from_secs(5), "{:?}", started.elapsed());
    assert_eq!(READ.load(std::sync::atomic::Ordering::SeqCst), 1, "stopped at the first file");
    assert!(!it.world.repo("z4c").join(PACKAGE_FILE).exists());
}

#[test]
fn ranges_and_includes_read_as_meant() {
    let v: TrackedVersion = "1.2.3".parse().unwrap();
    assert_eq!(range_of(&v, false), "~1.2.3");
    assert_eq!(range_of(&v, true), "=1.2.3");
    let t: TrackedVersion = "mixed-precision:1.2.3".parse().unwrap();
    assert_eq!(range_of(&t, false), "mixed-precision:~1.2.3");
    assert_eq!(range_of(&t, true), "mixed-precision:=1.2.3");
    for r in [range_of(&t, false), range_of(&v, false)] {
        let slug: Slug = format!("et/carpet@{r}").parse().unwrap();
        assert!(slug.req().unwrap().matches(&t) || slug.req().unwrap().matches(&v), "{r}");
    }
    assert_eq!(include_name("  #  include \"../a/b.h\" // x"), Some("../a/b.h"));
    assert_eq!(include_name("#include <carpet.hh>"), Some("carpet.hh"));
    assert_eq!(include_name("#include MACRO"), None);
    assert_eq!(include_name("// #include \"x.h\""), None);
    assert_eq!(lexical("arrangements/A/T/src/../../../B/U/x.h").as_deref(), Some("arrangements/B/U/x.h"));
    assert_eq!(lexical("a/../../x"), None);
}
