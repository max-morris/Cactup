//! `cactup package init` and `cactup stack init`, end to end: real upstream repos on disk, an
//! installation fetched from them and built (a config record and its `ThornList`), and a fake
//! Cactarium (`test_server`) answering `/lookup`, version lists, reads at a commit, `/whoami`,
//! `/repos`, a refresh and a stack release in the crate's shapes.
// Plan §8 (integration tests), the P7 row of §7's reviewer checks.

use super::*;
use crate::args::StackInitArgs;
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

/// A dependency claim on `target` at `range`, needing `needs`.
fn depends(target: &str, range: &str, needs: &[&str]) -> Value {
    json!({"kind": "dependency", "target": target, "range": range, "needs": needs, "note": null})
}

/// How the fake answers `POST /repos`.
#[derive(Clone, Copy, Default, PartialEq)]
enum Repos {
    /// A new entry: the preview with its fingerprint, then the registration.
    #[default]
    New,
    /// The package is registered: `existing`, with no fingerprint.
    Existing,
    /// A preview of a new entry without a fingerprint.
    NoExpect,
    /// A server from before `only` named a registered package.
    OldServer,
    /// The entry has no release tag yet.
    Untagged,
}

/// A URL as the installation names a repo after [`World::new`]: one anybody could fetch, unique
/// to its test (tests share the routes of the anonymous probe and of remote asks).
fn public(url: &str) -> String {
    if url.starts_with("https://up.test/") {
        return url.to_owned();
    }
    let mut parts = url.trim_end_matches('/').rsplit('/');
    let name = parts.next().unwrap_or(url).trim_end_matches(".git");
    let test = parts.nth(1).unwrap_or("x").trim_start_matches('.');
    format!("https://up.test/{test}/{name}.git")
}

/// The fake Cactarium.
#[derive(Clone, Default)]
struct Fake {
    /// Package id → its versions.
    packages: Vec<(String, Vec<Value>)>,
    /// URL → its package, the package's `fork-of` there, and whether it is a flesh.
    pins: Vec<(String, String, Vec<String>, bool)>,
    /// How `/repos` answers.
    repos: Repos,
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

    fn serve_logged(self) -> (String, Log) {
        self.serve()
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
                    .map(|p| match self.pins.iter().find(|(u, ..)| public(u) == public(&p.url)) {
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
                let refused = |message: String| {
                    let body = json!({"error": {"code": "bad-request", "message": message}}).to_string();
                    ("400 Bad Request", "api-generation: 1\r\n".to_owned(), body.into_bytes())
                };
                match self.repos {
                    Repos::OldServer => return refused(format!("No new entry named '{name}' to register (new here: )")),
                    Repos::Untagged => {
                        return refused(format!("'{name}' (cactarium.toml) can't be registered now: has none of its tags yet"))
                    }
                    _ => {}
                }
                let existing = self.repos == Repos::Existing;
                let mut entry = json!({"slug": format!("max/{name}"), "kind": "package", "file": "cactarium.toml",
                    "tag": "v1.0.0", "version": "1.0.0", "rule": "tags vX.Y.Z", "review": "pending",
                    "status": if existing { "existing" } else { "new" }});
                if !existing && !asked.dry_run {
                    assert_eq!(asked.expect.as_deref(), Some("fingerprint"), "registers what the preview showed");
                    entry["url"] = json!(format!("https://cactarium.test/max/{name}"));
                    entry["new-versions"] = json!(1);
                    entry["awaiting-review"] = json!(1);
                }
                let expect = (asked.dry_run && self.repos == Repos::New).then_some("fingerprint");
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
    tmp: tempfile::TempDir,
    ctx: Ctx,
    inst: Installation,
}

impl World {
    fn new(tmp: tempfile::TempDir, base: &str, cactarium: &str, stack: Option<&str>) -> World {
        let root = tmp.path().join("inst");
        let db = crate::database::Db::in_dir(&tmp.path().join("db"));
        let path = root.display().to_string();
        let stack_less = stack.is_none();
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
        let w = World { tmp, ctx, inst };
        // Fetched from local paths, named as anybody could fetch them (a local path is never sent
        // to Cactarium). The stack tests keep the paths: their repos' origins must match.
        if stack_less {
            w.publicize();
        }
        w
    }

    fn cactus(&self) -> PathBuf {
        self.inst.root.join("Cactus")
    }

    /// The pristine thornlist with every upstream path named by [`public`].
    fn publicize(&self) {
        let up = self.tmp.path().join("up").display().to_string();
        let pristine = std::fs::read_to_string(self.inst.source_thornlist()).unwrap();
        let named: Vec<String> = (thornlist::parse(&pristine).unwrap().components().iter())
            .filter_map(|c| c.url.clone())
            .filter(|u| u.starts_with(&up))
            .collect();
        let mut text = pristine;
        for url in named {
            text = text.replace(&format!("= {url}\n"), &format!("= {}\n", public(&url)));
        }
        std::fs::write(self.inst.source_thornlist(), text).unwrap();
    }

    /// The pristine thornlist with `url` named as written again (as [`World::publicize`] found it).
    fn unpublicize(&self, url: &str) {
        let pristine = std::fs::read_to_string(self.inst.source_thornlist()).unwrap();
        std::fs::write(self.inst.source_thornlist(), pristine.replace(&public(url), url)).unwrap();
    }

    /// The body of the lookup the command made.
    fn lookup_body(log: &Log) -> String {
        log.lock().unwrap().iter().find(|(p, _)| p == "/api/v1/lookup").map(|(_, b)| b.clone()).unwrap_or_default()
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

    fn out(&self) -> PathBuf {
        self.tmp.path().join("out")
    }

    fn stack_init(&self, force: bool) -> Res<()> {
        std::fs::create_dir_all(self.out()).unwrap();
        let args = StackInitArgs { dir: Some(self.out()), name: None, owner: None, force, publish: false, version: None };
        crate::commands::stack::init(&self.ctx, &args)
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
    let (cactarium, log) = Fake::default()
        .package("et/arith", vec![version("1.0.0", &"0".repeat(40), &aurl, &[("Arith", "Arith")], json!([]))])
        .pin(&aurl, "et/arith")
        .pin(&lurl, "et/loops")
        .serve_logged();
    let base = section(&lurl, &l, &["Loop/Loop"]) + &section(&aurl, &a, &["Arith/Arith"]) + &section(&zurl, &z, &["Z4c/Z4c"]);
    let w = World::new(tmp, &base, &cactarium, None);
    w.built(&["Loop/Loop", "Arith/Arith", "Z4c/Z4c"], &[]);
    // A local path is never sent to Cactarium: the repo is then no registered package's.
    w.unpublicize(&lurl);
    w.init("z4c", false, false).unwrap();
    let asked = World::lookup_body(&log);
    assert!(asked.contains(&public(&aurl)) && !asked.contains(&lurl) && !asked.contains("loops"), "{asked}");
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
                  # Depend on the one needed by these thorns, or on any of them:\n\
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
    assert!(
        text.contains(
            "# TODO: the clone of repo carpetmp is shallow.\n# Its history cannot show whether it forks et/carpet.\n\
             # Run `git fetch --unshallow` in it, then `cactup package init carpetmp -f`.\n"
        ),
        "{text}"
    );
    assert!(gix::open(w.repo("carpetmp")).unwrap().is_shallow(), "cactup's clone is left as it was");
    let dir = w.repo("carpetmp");
    assert_eq!(
        shallow_notice("carpetmp", &dir),
        [
            "The clone of repo carpetmp is shallow: its history cannot show a fork.".to_owned(),
            format!("Fetch its history with `git -C {} fetch --unshallow`.", dir.display()),
            "Then run `cactup package init carpetmp -f`.".to_owned(),
        ]
    );
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
        // A note that would start a claim of its own, were it written as it is.
        m.claim_overrides.insert(
            "drop et/a:incompatible-with:et/b".into(),
            crate::installation::ClaimOverride {
                kind: "drop".into(),
                note: Some("x\n[[fork-of]]\ntarget = \"et/evil@=1.0.0\"".into()),
            },
        );
    });
    it.world.init("z4c", false, false).unwrap();
    let (text, _) = it.world.manifest("z4c");
    assert!(text.contains("#   satisfies local:carpetx et/carpetx  (our fork)\n"), "{text}");
    let (_, m) = it.world.manifest("z4c");
    assert!(m.fork_of.is_empty(), "{text}");
    assert!(text.contains("#   drop et/a:incompatible-with:et/b  (x [[fork-of]] target = \"et/evil@=1.0.0\")\n"), "{text}");
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

/// Tags HEAD of the repo at `dir` as `name`.
fn tag(dir: &Path, name: &str) {
    let mut repo = gix::open(dir).unwrap();
    let _ = repo.committer_or_set_generic_fallback();
    let head = repo.head_id().unwrap().detach();
    repo.reference(format!("refs/tags/{name}"), head, gix::refs::transaction::PreviousValue::Any, "test").unwrap();
}

/// A repo z4c to publish, at `https://up.test/z4c.git` (served to the anonymous probe, its tags
/// asked of the upstream repo), with Cactarium answering `/repos` as `repos` says. The repo's
/// clone, its upstream, the manifest's text and the request log.
struct Publishing {
    world: World,
    dir: PathBuf,
    up: PathBuf,
    log: Log,
}

const MANIFEST: &str = "format = 1\n[package]\nname = \"z4c\"\nowners = [\"max\"]\n";
const FILES: [(&str, &str); 1] = [("Z4c/interface.ccl", "IMPLEMENTS: z4c\n")];

fn publishing(repos: Repos) -> Publishing {
    use crate::fetch::anonymous::testserver::{serve, Mode};
    serve("up.test", Mode::Public);
    let tmp = tempfile::tempdir().unwrap();
    let (zurl, z) = upstream(tmp.path(), "z4c", &FILES);
    crate::fetch::git::ASK_ROUTES.lock().unwrap().insert(public(&zurl), zurl.clone());
    let (cactarium, log) = Fake { repos, ..Fake::default() }.serve();
    let world = World::new(tmp, &section(&zurl, &z, &["Z4c/Z4c"]), &cactarium, None);
    let dir = world.repo("z4c");
    Publishing { world, dir, up: PathBuf::from(zurl), log }
}

impl Publishing {
    fn publish(&self) -> Res<()> {
        self.world.publish("z4c")
    }

    fn refused(&self) -> String {
        format!("{:#}", self.publish().unwrap_err())
    }

    /// The manifest committed in the clone and, as a push would, in the upstream: the same commit.
    fn commit_both(&self, manifest: &str) {
        let mut tree = FILES.to_vec();
        tree.push((PACKAGE_FILE, manifest));
        let here = crate::fetch::tests::commit_files(&self.dir, &tree);
        let there = crate::fetch::tests::commit_files(&self.up, &tree);
        assert_eq!(here, there, "one commit, made twice");
    }

    fn asked(&self, path: &str) -> Vec<String> {
        self.log.lock().unwrap().iter().filter(|(p, _)| p == path).map(|(_, b)| b.clone()).collect()
    }
}

/// `--publish` writes nothing and reads what Cactarium reads, a release tag on the remote: it
/// refuses a repo with no release tag, a tag not pushed, and a manifest that differs from the
/// one at the newest pushed tag, each naming the git commands; a newer tag not pushed is said,
/// the pushed one used; HEAD's own unpushed work does not matter. Then it registers the package
/// (a preview, then exactly what it showed, at the https URL anybody can fetch).
#[test]
fn publish_reads_the_newest_pushed_release_tag_then_registers() {
    let it = publishing(Repos::New);
    let dir = it.dir.display().to_string();
    let err = it.refused();
    assert!(err.contains("has no cactarium.toml") && err.contains("`cactup package init z4c`"), "{err}");

    std::fs::write(it.dir.join(PACKAGE_FILE), MANIFEST).unwrap();
    let err = it.refused();
    assert!(err.contains("Repo z4c has no release tag.\nCommit cactarium.toml, tag a release and push both:"), "{err}");
    for step in ["add cactarium.toml", "commit -m \"Add cactarium.toml\"", "tag v1.0.0", "push", "push origin v1.0.0"] {
        assert!(err.contains(&format!("\n  git -C {dir} {step}")), "{step}: {err}");
    }

    it.commit_both(MANIFEST);
    tag(&it.dir, "v1.0.0");
    let err = it.refused();
    assert!(err.contains(&format!("Tag v1.0.0 of repo z4c is not on its remote.\nPush it:\n  git -C {dir} push origin v1.0.0")), "{err}");

    tag(&it.up, "v1.0.0");
    std::fs::write(it.dir.join(PACKAGE_FILE), format!("{MANIFEST}# edited\n")).unwrap();
    let err = it.refused();
    assert!(err.contains("cactarium.toml in repo z4c differs from the one at tag v1.0.0.\nCommit cactarium.toml, tag a release and push both:"), "{err}");
    assert!(err.contains("commit -m \"Update cactarium.toml\" cactarium.toml") && err.contains("tag v1.0.1"), "{err}");
    // Committed already: only the tag is missing, and the header says so.
    let mut tree = FILES.to_vec();
    let edited = format!("{MANIFEST}# edited\n");
    tree.push((PACKAGE_FILE, edited.as_str()));
    crate::fetch::tests::commit_files(&it.dir, &tree);
    let err = it.refused();
    assert!(
        err.contains(&format!("differs from the one at tag v1.0.0.\nTag a release and push it:\n  git -C {dir} tag v1.0.1\n")),
        "{err}"
    );
    assert!(!err.contains("commit -m"), "{err}");
    assert!(it.asked("/api/v1/repos").is_empty(), "nothing asked before the tag holds the manifest");

    // Committed on top and tagged here only: the pushed tag is what counts.
    std::fs::write(it.dir.join(PACKAGE_FILE), MANIFEST).unwrap();
    let mut tree = FILES.to_vec();
    tree.extend([(PACKAGE_FILE, MANIFEST), ("NEWS", "unpushed\n")]);
    crate::fetch::tests::commit_files(&it.dir, &tree);
    tag(&it.dir, "v1.1.0");
    it.publish().unwrap_or_else(|e| panic!("{e:#}"));
    let repos = it.asked("/api/v1/repos");
    assert_eq!(repos.len(), 2, "a preview, then the registration");
    assert!(repos[0].contains(&format!("\"url\":\"{}\"", public(&it.up.display().to_string()))), "{}", repos[0]);
    assert!(repos[0].contains("\"only\":[\"z4c\"]") && repos[0].contains("\"dry-run\":true"), "{}", repos[0]);
    assert!(repos[1].contains("\"expect\":\"fingerprint\"") && !repos[1].contains("dry-run"), "{}", repos[1]);
    assert_eq!(std::fs::read_to_string(it.dir.join(PACKAGE_FILE)).unwrap(), MANIFEST, "--publish writes nothing");
}

/// A pushed release tag holding the manifest on disk, ready to publish.
fn published(repos: Repos) -> Publishing {
    let it = publishing(repos);
    std::fs::write(it.dir.join(PACKAGE_FILE), MANIFEST).unwrap();
    it.commit_both(MANIFEST);
    tag(&it.dir, "v1.0.0");
    tag(&it.up, "v1.0.0");
    it
}

/// A registered package (`existing`, with no fingerprint, as the server answers a name in
/// `only` that is registered) is refreshed, never registered again.
#[test]
fn publish_refreshes_a_registered_package() {
    let it = published(Repos::Existing);
    it.publish().unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(it.asked("/api/v1/repos").len(), 1, "the preview only");
    assert_eq!(it.asked("/api/v1/packages/max/z4c/refresh").len(), 1);
}

/// A preview of a new entry without a fingerprint registers nothing.
#[test]
fn publish_never_registers_unguarded() {
    let it = published(Repos::NoExpect);
    let err = it.refused();
    assert!(err.contains("came without a fingerprint, so nothing was registered"), "{err}");
    assert_eq!(it.asked("/api/v1/repos").len(), 1, "the preview only");
}

/// A server from before `only` named a registered package answers 400 "No new entry named …":
/// said as a server too old to refresh this way, never taken as `existing`.
#[test]
fn publish_says_an_old_server_cannot_refresh_this_way() {
    let it = published(Repos::OldServer);
    let err = it.refused();
    assert!(err.contains("is too old to refresh a registered package this way; it needs an update."), "{err}");
    assert!(err.contains("run `cactup refresh OWNER/z4c`") && !err.contains("new here"), "{err}");
    assert!(it.asked("/api/v1/packages/max/z4c/refresh").is_empty());
}

/// A refusal about tags carries the way past.
#[test]
fn publish_says_how_past_a_refusal_about_tags() {
    let it = published(Repos::Untagged);
    let err = it.refused();
    assert!(err.contains("'z4c' (cactarium.toml) can't be registered now: has none of its tags yet\nTag a release and push the tag."), "{err}");
}

/// `--owner` must be an owner's name; nothing is asked otherwise.
#[test]
fn publish_checks_the_owner() {
    let it = published(Repos::New);
    let args = PackageInitArgs {
        repo: Some("z4c".into()),
        name: None,
        owner: Some("Not An Owner".into()),
        exact: false,
        force: false,
        publish: true,
    };
    let err = format!("{:#}", init(&it.world.ctx, &args).unwrap_err());
    assert!(err.contains("\"Not An Owner\" is not a Cactarium owner"), "{err}");
    assert!(it.log.lock().unwrap().is_empty());
}

/// A remote nobody else could fetch (a local path) is never registered.
#[test]
fn publish_refuses_a_local_remote() {
    let it = published(Repos::New);
    it.world.unpublicize(&it.up.display().to_string());
    assert!(it.publish().is_err());
    assert!(it.asked("/api/v1/repos").is_empty());
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
    // One thorn, with many sources: the scan stops after the first, not after all of them.
    std::fs::remove_dir_all(it.world.repo("z4c").join("Extra")).unwrap();
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
    assert_eq!(CRATE_SCANS.load(std::sync::atomic::Ordering::SeqCst), 0, "the crate's scan, which takes no flag, never started");
    assert!(!it.world.repo("z4c").join(PACKAGE_FILE).exists());
}

/// Runs `test` again in a child process with `env` set, and checks it passed there: a test that
/// sets the process's interrupt flag.
fn in_child(test: &str, env: &str) -> bool {
    if std::env::var_os(env).is_some() {
        return false;
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([test, "--exact", "--test-threads=1"])
        .env(env, "1")
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success() && said.contains("1 passed"), "{said}{}", String::from_utf8_lossy(&out.stderr));
    true
}

/// A Ctrl-C after the last request, just before `package init` writes, fails it with nothing
/// written.
#[test]
fn a_late_interrupt_writes_no_manifest() {
    if in_child("commands::package::tests::a_late_interrupt_writes_no_manifest", "CACTUP_TEST_LATE_INTERRUPT") {
        return;
    }
    let it = z4c();
    BEFORE_WRITE.set(|| gix::interrupt::IS_INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst)).unwrap();
    let err = format!("{:#}", it.world.init("z4c", false, false).unwrap_err());
    assert!(err.contains("Interrupted"), "{err}");
    assert!(!it.world.repo("z4c").join(PACKAGE_FILE).exists());
}

/// The same for `stack init`: neither file is written.
#[test]
fn a_late_interrupt_writes_no_stack() {
    if in_child("commands::package::tests::a_late_interrupt_writes_no_stack", "CACTUP_TEST_LATE_STACK_INTERRUPT") {
        return;
    }
    let it = stacked("et/base@1.0.0");
    BEFORE_WRITE.set(|| gix::interrupt::IS_INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst)).unwrap();
    let err = format!("{:#}", it.world.stack_init(false).unwrap_err());
    assert!(err.contains("Interrupted"), "{err}");
    assert!(!it.world.out().join("cactarium-stack.toml").exists());
    assert!(!it.world.out().join("cactarium-stack.th").exists());
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

/// The stack installs: et/base@1.0.0 (carpetx, z4c), carpetx moved since; three packages added
/// (one replacing z4c, one beside it, one at its branch tip), the one beside it depending on
/// et/carpetx.
struct Stacked {
    world: World,
    log: Log,
    c2: String,
    e: String,
    t: String,
    z: String,
}

fn stacked(stack: &str) -> Stacked {
    stacked_with(stack, "")
}

/// [`stacked`], with `extra` appended to the release's thornlist.
fn stacked_with(stack: &str, extra: &str) -> Stacked {
    let tmp = tempfile::tempdir().unwrap();
    let carpetx = [("CarpetX/interface.ccl", "IMPLEMENTS: carpetx\n"), ("Loop/interface.ccl", "IMPLEMENTS: loop\n")];
    let (curl, c1) = upstream(tmp.path(), "carpetx", &carpetx);
    let mut later = carpetx.to_vec();
    later.push(("NEWS", "moved\n"));
    let (_, c2) = upstream(tmp.path(), "carpetx", &later);
    let (zurl, z) = upstream(tmp.path(), "z4c", &[("Z4c/interface.ccl", "IMPLEMENTS: z4c\n")]);
    let (e, t, zm) = ("e".repeat(40), "f".repeat(40), "a".repeat(40));
    let release_text =
        format!("!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n{}{}", section(&curl, &c1, &["CarpetX/CarpetX", "CarpetX/Loop"]), section(&zurl, &z, &["Z4c/Z4c"]))
            + extra;
    let frozen = json!({"format": 1, "stack": "et/base@1.0.0", "root": "Cactus",
        "thornlist": {"filename": "base.th", "text": release_text},
        "repo": [{"name": "carpetx", "url": curl, "ref": "main", "commit": c1}, {"name": "z4c", "url": zurl, "commit": "d".repeat(40)}]});
    let mut fake = Fake::default()
        .package("et/carpetx", vec![version("1.1.0", &c2, &curl, &[("CarpetX", "CarpetX"), ("CarpetX", "Loop")], json!([]))])
        .package("max/z4c-mp", vec![version("1.0.0", &zm, "https://x/z4c-mp.git", &[("Z4c", "Z4c")], json!([]))])
        .package(
            "max/extra",
            vec![version("2.0.0", &e, "https://x/extra.git", &[("Extra", "Extra")], json!([depends("et/carpetx", "^1.0", &["Loop"])]))],
        )
        .pin(&curl, "et/carpetx");
    fake.stacks.push(("et/base".into(), frozen));
    let (cactarium, log) = fake.serve();
    // Installed at carpetx's newer commit.
    let base = section(&curl, &c2, &["CarpetX/CarpetX", "CarpetX/Loop"]) + &section(&zurl, &z, &["Z4c/Z4c"]);
    let w = World::new(tmp, &base, &cactarium, Some(stack));
    w.edit_meta(|m| {
        m.added.push(added("max/z4c-mp", "1.0.0", Some("v1.0.0"), &zm, "z4c-mp", "Z4c/Z4c", &["z4c"]));
        m.added.push(added("max/extra", "2.0.0", Some("v2.0.0"), &e, "extra", "Extra/Extra", &[]));
        m.added.push(added("max/tipper", "head", Some("main"), &t, "tipper", "Tip/Tip", &[]));
    });
    Stacked { world: w, log, c2, e, t, z }
}

/// An `[[added]]` record: at `https://x/<repo>.git`, checking out `thorn`, replacing `replaces`.
fn added(package: &str, version: &str, tag: Option<&str>, commit: &str, repo: &str, thorn: &str, replaces: &[&str]) -> AddedPackage {
    AddedPackage {
        package: package.into(),
        version: version.into(),
        tag: tag.map(Into::into),
        commit: commit.into(),
        url: format!("https://x/{repo}.git"),
        repo: repo.into(),
        thorns: vec![AddedThorn { checkout: thorn.into(), repo_path: "$2".into() }],
        replaces: replaces.iter().map(|r| (*r).to_owned()).collect(),
        contact: None,
        libraries: Vec::new(),
        flesh: Vec::new(),
    }
}

/// `stack init`: extends the release the installation is on, pins the base repo moved since,
/// an override (at the exact version) for the package replacing a base repo, pinned sections of
/// cactarium-stack.th for the others (the tip at its fetched commit, never a version), and the
/// dependency an installed package declares, as information naming it. Both files parse.
#[test]
fn stack_init_extends_pins_overrides_and_writes_sections() {
    let it = stacked("et/base@1.0.0");
    let w = &it.world;
    w.stack_init(false).unwrap_or_else(|e| panic!("{e:#}"));
    let text = std::fs::read_to_string(w.out().join("cactarium-stack.toml")).unwrap();
    let m = Manifest::from_repo_file(cactup_ecosystem_common::manifest::STACK_FILE, &text).unwrap_or_else(|e| panic!("{e}\n{text}"));
    let s = m.stack.as_ref().unwrap();
    assert_eq!(s.extends.iter().map(ToString::to_string).collect::<Vec<_>>(), ["et/base@1.0.0"], "{text}");
    assert_eq!((s.name.as_str(), s.owners[0].as_str()), ("t", "max"));
    assert_eq!(s.pins.len(), 1, "{text}");
    assert_eq!((s.pins[0].name.as_str(), s.pins[0].commit.as_deref()), ("carpetx", Some(it.c2.as_str())), "{text}");
    assert_eq!(s.overrides.len(), 1, "{text}");
    assert_eq!(s.overrides[0].repo, "z4c");
    assert_eq!(s.overrides[0].package.as_ref().unwrap().to_string(), "max/z4c-mp@=1.0.0");
    assert_eq!(s.thornlist.as_deref(), Some("cactarium-stack.th"));
    assert_eq!(m.dependencies.len(), 1, "{text}");
    assert_eq!(m.dependencies[0].target.as_ref().unwrap().to_string(), "et/carpetx@^1.0");
    assert_eq!(m.dependencies[0].needs.as_deref(), Some(&["Loop".to_owned()][..]));
    assert!(text.contains("# Declared by max/extra@2.0.0.\n[[dependency]]"), "{text}");

    let th = std::fs::read_to_string(w.out().join("cactarium-stack.th")).unwrap();
    let list = thornlist::parse(&th).unwrap_or_else(|e| panic!("{e:#}\n{th}"));
    let pinned = |checkout: &str| list.components().iter().find(|c| c.checkout == checkout).and_then(|c| c.commit.clone());
    assert_eq!(pinned("Extra/Extra").as_deref(), Some(it.e.as_str()), "{th}");
    assert_eq!(pinned("Tip/Tip").as_deref(), Some(it.t.as_str()), "a tip is pinned at its commit: {th}");
    assert!(list.components().iter().all(|c| c.checkout != "Z4c/Z4c"), "the replacing package is an override: {th}");

    let err = format!("{:#}", w.stack_init(false).unwrap_err());
    assert!(err.contains("exists") && err.contains("-f"), "{err}");
    w.stack_init(true).unwrap();
}

/// A base at a stack's branch tip has no release to extend: refused, naming the way onto one.
#[test]
fn stack_init_refuses_a_tip() {
    let it = stacked("et/base@head");
    let err = format!("{:#}", it.world.stack_init(false).unwrap_err());
    assert!(err.contains("et/base@head, which is no release"), "{err}");
    assert!(err.contains("`cactup inst refetch --release et/base@<version>`"), "{err}");
    assert!(!it.world.out().join("cactarium-stack.toml").exists());
}

/// Uncommitted changes in a repo of the installation are refused, as export refuses them.
#[test]
fn stack_init_refuses_a_dirty_repo() {
    let it = stacked("et/base@1.0.0");
    let w = &it.world;
    std::fs::write(w.repo("carpetx").join("CarpetX/interface.ccl"), "IMPLEMENTS: carpetx\n# edited\n").unwrap();
    let err = format!("{:#}", w.stack_init(false).unwrap_err());
    assert!(err.contains("These repos cannot go into a stack"), "{err}");
    assert!(err.contains("carpetx: uncommitted changes in the installation; commit them"), "{err}");
    assert!(!w.out().join("cactarium-stack.toml").exists());
}

/// A thorn in an ignore section is refused, as export refuses it: the installation's own, with
/// the way out; the release's, as what Cactarium refuses a stack extending. An empty
/// `!CHECKOUT` (ET's placeholder for private thorns) is no thorn.
#[test]
fn stack_init_refuses_ignore_components() {
    const IGNORE: &str = "\n!TARGET = $ROOT\n!TYPE = ignore\n!CHECKOUT = Private/Secret\n";
    const EMPTY: &str = "\n!TARGET = $ROOT\n!TYPE = ignore\n!CHECKOUT =\n";
    let it = stacked_with("et/base@1.0.0", EMPTY);
    let w = &it.world;
    let pristine = std::fs::read_to_string(w.inst.source_thornlist()).unwrap();
    std::fs::write(w.inst.source_thornlist(), format!("{pristine}{EMPTY}")).unwrap();
    w.stack_init(false).unwrap_or_else(|e| panic!("an empty !CHECKOUT is no thorn: {e:#}"));
    std::fs::write(w.inst.source_thornlist(), format!("{pristine}{IGNORE}")).unwrap();
    let err = format!("{:#}", w.stack_init(true).unwrap_err());
    assert!(err.starts_with("These thorns cannot go into a stack:\n  Private/Secret: an ignore component, which nobody else can fetch; put it in a repo and register that with Cactarium"), "{err}");
    assert!(!err.contains("et/base"), "{err}");

    let it = stacked_with("et/base@1.0.0", IGNORE);
    let w = &it.world;
    let pristine = std::fs::read_to_string(w.inst.source_thornlist()).unwrap();
    std::fs::write(w.inst.source_thornlist(), format!("{pristine}{IGNORE}")).unwrap();
    let err = format!("{:#}", w.stack_init(false).unwrap_err());
    assert_eq!(
        err,
        "These thorns cannot go into a stack:\n  Private/Secret: an ignore component of et/base@1.0.0, which nobody else can \
         fetch; Cactarium refuses a stack extending it",
        "said once, as the release's"
    );
    assert!(!w.out().join("cactarium-stack.toml").exists());
}

/// `stack init --publish --version` writes nothing and uploads the definition and its thornlist.
#[test]
fn stack_publish_uploads_the_definition() {
    let it = stacked("et/base@1.0.0");
    let w = &it.world;
    w.stack_init(false).unwrap();
    let before = std::fs::read_to_string(w.out().join("cactarium-stack.toml")).unwrap();
    let args = StackInitArgs { dir: Some(w.out()), name: None, owner: None, force: false, publish: true, version: Some("1.0.0".into()) };
    crate::commands::stack::init(&w.ctx, &args).unwrap_or_else(|e| panic!("{e:#}"));
    let asked = it.log.lock().unwrap().clone();
    let (_, body) = asked.iter().find(|(p, _)| p == "/api/v1/stacks/max/t/releases").expect("published");
    let request: cactup_ecosystem_common::api::ReleaseRequest = serde_json::from_str(body).unwrap();
    assert_eq!(request.definition, before);
    assert_eq!(request.version, "1.0.0");
    assert_eq!(request.thornlist.as_deref(), Some(std::fs::read_to_string(w.out().join("cactarium-stack.th")).unwrap().as_str()));
    assert_eq!(std::fs::read_to_string(w.out().join("cactarium-stack.toml")).unwrap(), before, "--publish writes nothing");
}

/// The built thorn wins over an unbuilt one meeting the same requirement; a requirement only
/// unbuilt thorns meet is met by them.
#[test]
fn built_providers_come_first_and_unbuilt_ones_after() {
    let it = z4c();
    let w = &it.world;
    // Another package implementing loop, installed but not built.
    let (ourl, o) = upstream(w.tmp.path(), "loops", &[("Loop2/interface.ccl", "IMPLEMENTS: loop\n")]);
    let pristine = std::fs::read_to_string(w.inst.source_thornlist()).unwrap();
    std::fs::write(w.inst.source_thornlist(), format!("{pristine}{}", section(&public(&ourl), &o, &["Loops/Loop2"]))).unwrap();
    let loop2 = w.cactus().join("arrangements/Loops/Loop2");
    std::fs::create_dir_all(&loop2).unwrap();
    std::fs::write(loop2.join("interface.ccl"), "IMPLEMENTS: loop\n").unwrap();
    w.init("z4c", false, false).unwrap_or_else(|e| panic!("{e:#}"));
    let (text, m) = w.manifest("z4c");
    assert_eq!(m.dependencies.len(), 1, "{text}");
    assert_eq!(m.dependencies[0].target.as_ref().unwrap().to_string(), "et/carpetx@~1.1.0", "{text}");

    // Neither built: both meet it, a TODO any-of.
    w.built(&["CarpetX/CarpetX", "Z4c/Z4c"], &[]);
    w.init("z4c", false, true).unwrap();
    let (text, m) = w.manifest("z4c");
    assert!(m.dependencies.is_empty() && text.contains("several installed packages provide it"), "{text}");
}

/// A requirement met by no built thorn but by one installed is met by it.
#[test]
fn an_unbuilt_provider_is_a_provider() {
    let it = z4c();
    it.world.built(&["CarpetX/CarpetX", "Z4c/Z4c"], &[]);
    it.world.init("z4c", false, false).unwrap();
    let (text, m) = it.world.manifest("z4c");
    assert_eq!(m.dependencies.len(), 1, "{text}");
    assert_eq!(m.dependencies[0].needs.as_deref(), Some(&["Loop".to_owned()][..]), "{text}");
}

/// The repo itself is asked about where its clone is, whatever the build recorded for it.
#[test]
fn the_repo_itself_is_asked_at_its_head() {
    let it = z4c();
    let (_, head) = crate::fetch::git::head_of(&it.world.repo("z4c")).unwrap();
    it.world.built(&["CarpetX/CarpetX", "CarpetX/Loop", "Z4c/Z4c"], &[("z4c", &"0".repeat(40)), ("carpetx", &it.c1)]);
    it.world.init("z4c", false, false).unwrap();
    let asked: cactup_ecosystem_common::api::LookupRequest = serde_json::from_str(&World::lookup_body(&it.log)).unwrap();
    let up = it.world.tmp.path().join("up");
    let pin = asked.pins.iter().find(|p| p.url == public(&up.join("z4c").display().to_string())).expect("the repo is asked about");
    assert_eq!(pin.commit, head.to_string());
    let carpetx = asked.pins.iter().find(|p| p.url == public(&up.join("carpetx").display().to_string())).unwrap();
    assert_eq!(carpetx.commit, it.c1, "another repo, as built");
}

/// A requirement met only by another clone of the repo's own package is no dependency.
#[test]
fn no_package_depends_on_itself() {
    let tmp = tempfile::tempdir().unwrap();
    let (xurl, x) = upstream(tmp.path(), "z4cloops", &[("Loop/interface.ccl", "IMPLEMENTS: loop\n")]);
    let (zurl, z) = upstream(tmp.path(), "z4c", &[("Z4c/interface.ccl", "IMPLEMENTS: z4c\nINHERITS: loop\n")]);
    let (cactarium, _) = Fake::default()
        .package("max/z4c", vec![version("1.0.0", &x, &xurl, &[("Z4c", "Loop")], json!([])), version("1.0.1", &z, &zurl, &[("Z4c", "Z4c")], json!([]))])
        .pin(&xurl, "max/z4c")
        .pin(&zurl, "max/z4c")
        .serve();
    let w = World::new(tmp, &(section(&xurl, &x, &["Z4c/Loop"]) + &section(&zurl, &z, &["Z4c/Z4c"])), &cactarium, None);
    w.built(&["Z4c/Loop", "Z4c/Z4c"], &[]);
    w.init("z4c", false, false).unwrap();
    let (text, m) = w.manifest("z4c");
    assert!(m.dependencies.is_empty(), "{text}");
    assert!(
        text.contains("# TODO: implementation loop, for Z4c: provided by Z4c/Loop of max/z4c, this repo's own package.\n"),
        "{text}"
    );
}

/// A provider at a version cactup cannot read is a TODO, never dropped.
#[test]
fn an_unreadable_version_is_a_todo() {
    let tmp = tempfile::tempdir().unwrap();
    let (lurl, l) = upstream(tmp.path(), "loops", &[("Loop/interface.ccl", "IMPLEMENTS: loop\n")]);
    let (zurl, z) = upstream(tmp.path(), "z4c", &[("Z4c/interface.ccl", "IMPLEMENTS: z4c\nINHERITS: loop\n")]);
    let (cactarium, _) = Fake::default()
        .package("et/loops", vec![version("banana", &l, &lurl, &[("Loop", "Loop")], json!([]))])
        .pin(&lurl, "et/loops")
        .serve();
    let w = World::new(tmp, &(section(&lurl, &l, &["Loop/Loop"]) + &section(&zurl, &z, &["Z4c/Z4c"])), &cactarium, None);
    w.built(&["Loop/Loop", "Z4c/Z4c"], &[]);
    w.init("z4c", false, false).unwrap();
    let (text, m) = w.manifest("z4c");
    assert!(m.dependencies.is_empty(), "{text}");
    assert!(text.contains("# TODO: implementation loop, for Z4c: provided by Loop/Loop of et/loops@banana.\n# cactup cannot read that version.\n"), "{text}");
}

/// A config built with --virtual-executable has no ThornList: said, with the way past.
#[test]
fn a_virtual_build_says_what_to_do() {
    let it = z4c();
    std::fs::remove_file(it.world.cactus().join("configs/sim/ThornList")).unwrap();
    let err = format!("{:#}", it.world.init("z4c", false, false).unwrap_err());
    assert!(err.contains("Config sim has no ThornList.\nA build with --virtual-executable writes none."), "{err}");
    assert!(err.contains("`cactup build sim`"), "{err}");
}

/// The stack init's files, both checked to parse.
fn stack_files(w: &World) -> (String, Manifest, Option<Thornlist>) {
    let text = std::fs::read_to_string(w.out().join("cactarium-stack.toml")).unwrap();
    let m = Manifest::from_repo_file(cactup_ecosystem_common::manifest::STACK_FILE, &text).unwrap_or_else(|e| panic!("{e}\n{text}"));
    let th = std::fs::read_to_string(w.out().join("cactarium-stack.th")).ok().map(|t| thornlist::parse(&t).unwrap());
    (text, m, th)
}

/// An override redirects every checkout of a repo, so only a package that takes the place of
/// every checkout the release has from it is one (here a tip: its URL and branch, pinned at its
/// commit). A package replacing only some checkouts is a section, as in the installation: the
/// release's other checkouts of that repo stay where they are.
#[test]
fn stack_init_overrides_only_a_whole_repo() {
    let it = stacked("et/base@1.0.0");
    let w = &it.world;
    let cx = "c".repeat(40);
    w.edit_meta(|m| {
        m.added = vec![
            added("max/cx-mp", "1.0.0", Some("v1.0.0"), &cx, "cx-mp", "CarpetX/CarpetX", &["carpetx"]),
            added("max/tipper", "head", Some("main"), &it.t, "tipper", "Z4c/Z4c", &["z4c"]),
        ]
    });
    w.stack_init(false).unwrap_or_else(|e| panic!("{e:#}"));
    let (text, m, th) = stack_files(w);
    let s = m.stack.as_ref().unwrap();
    assert_eq!(s.overrides.len(), 1, "{text}");
    let o = &s.overrides[0];
    assert_eq!((o.repo.as_str(), o.url.as_deref(), o.git_ref.as_deref()), ("z4c", Some("https://x/tipper.git"), Some("main")), "{text}");
    assert!(o.package.is_none(), "{text}");
    let pinned: Vec<(&str, Option<&str>)> = s.pins.iter().map(|p| (p.name.as_str(), p.commit.as_deref())).collect();
    assert!(pinned.contains(&("z4c", Some(it.t.as_str()))), "the tip at its commit: {text}");
    assert!(pinned.contains(&("carpetx", Some(it.c2.as_str()))), "carpetx's other checkouts stay, moved: {text}");
    let th = th.expect("a section for the partial replacement");
    let carpetx = th.components().iter().find(|c| c.checkout == "CarpetX/CarpetX").expect("CarpetX/CarpetX from the package");
    assert_eq!((carpetx.url.as_deref(), carpetx.commit.as_deref()), (Some("https://x/cx-mp.git"), Some(cx.as_str())), "{text}");
    assert!(th.components().iter().all(|c| c.checkout != "CarpetX/Loop"), "Loop is the release's still");
    assert!(!text.contains("TODO"), "{text}");
}

/// A package checked out at a commit of its own (committed work on top of its version) is
/// pinned there, never named by the version, which would drop that work. With its clone named
/// as the repo it replaces, the release's pin for that repo is left to the override's.
#[test]
fn stack_init_pins_committed_work_in_an_added_package() {
    let it = stacked("et/base@1.0.0");
    let w = &it.world;
    // max/z4c-mp's clone is the base's z4c clone, moved to its own repository.
    let fork = "https://x/z4c-fork.git";
    crate::fetch::git::set_origin_url(&w.repo("z4c"), fork).unwrap();
    w.edit_meta(|m| {
        let mp = m.added.iter_mut().find(|a| a.package == "max/z4c-mp").unwrap();
        mp.repo = "z4c".into();
        mp.url = fork.into();
    });
    w.stack_init(false).unwrap_or_else(|e| panic!("{e:#}"));
    let (text, m, _) = stack_files(w);
    let s = m.stack.as_ref().unwrap();
    let o = s.overrides.iter().find(|o| o.repo == "z4c").expect("an override");
    assert_eq!((o.url.as_deref(), o.git_ref.as_deref(), o.package.as_ref()), (Some(fork), None, None), "{text}");
    let z4c: Vec<_> = s.pins.iter().filter(|p| p.name == "z4c").collect();
    assert_eq!(z4c.len(), 1, "one pin: {text}");
    assert_eq!(z4c[0].commit.as_deref(), Some(it.z.as_str()), "{text}");
}

/// The same for a package written as a section: pinned at the commit installed, with no tag.
#[test]
fn stack_init_pins_committed_work_in_a_section() {
    let it = stacked("et/base@1.0.0");
    let w = &it.world;
    let (xurl, x) = upstream(w.tmp.path(), "extra", &[("Extra/interface.ccl", "IMPLEMENTS: extra\n")]);
    copy_tree(Path::new(&xurl), &w.repo("extra"));
    crate::fetch::git::set_origin_url(&w.repo("extra"), "https://x/extra.git").unwrap();
    mark_pushed(&w.repo("extra"));
    w.stack_init(false).unwrap_or_else(|e| panic!("{e:#}"));
    let (text, _, th) = stack_files(w);
    let th = th.unwrap();
    let extra = th.components().iter().find(|c| c.checkout == "Extra/Extra").unwrap();
    assert_eq!((extra.commit.as_deref(), extra.branch.as_deref()), (Some(x.as_str()), None), "{text}");
    assert_ne!(x, it.e);
}

/// Counts in progress phases read as counts: `1 repo`, `2 repos`.
#[test]
fn counts_read_as_counts() {
    use crate::commands::resolution::counted;
    assert_eq!((counted(1, "repo"), counted(0, "repo"), counted(2, "package")), ("1 repo".into(), "0 repos".into(), "2 packages".into()));
}
