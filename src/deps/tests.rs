//! Table tests: every directory under `fixtures/cactarium/` is one case.
//!
//! - `versions/*.json`: `GET /packages/{o}/{n}/versions` answers (`PackageVersions`), as the
//!   Cactarium serves them. A version may carry `"review": "pending"` (S6, plan §6), which the
//!   crate's `VersionInfo` at 2f2c8f4 does not model yet and so ignores; this reads it.
//! - `snapshots/*.json` (optional): `GET /packages/{o}/{n}/versions/{commit|head|master}`
//!   answers, as `{"slug", "at", "untrusted", "served": PackageVersion}`.
//! - `case.json`: the request (specs, present packages, options, ignores, overrides, the
//!   providers `GET /providers` answers with) or a removal, and what must come out.
//!
//! `ServerRules` mirrors the Cactarium's dependency check (`deps.rs`: `Need`, `needs_of`,
//! `meets`) until the crate carries it (S7).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cactup_ecosystem_common::api::{ClaimInfo, PackageVersion, ThornInfo};
use cactup_ecosystem_common::ccl::ThornFacts;
use serde::Deserialize;
use serde_json::Value;

use super::*;

/// The Cactarium's requirement semantics today: implementations inherited or befriended,
/// capabilities and thorns required, functions required; names compared case-insensitively.
struct ServerRules;

impl Requirements for ServerRules {
    fn of(&self, f: &ThornFacts) -> Vec<Requirement> {
        let mut v: Vec<Requirement> = Vec::new();
        v.extend(f.inherits.iter().chain(&f.friends).map(|i| Requirement::Implementation(i.clone())));
        v.extend(f.requires.iter().map(|c| Requirement::Capability(c.clone())));
        v.extend(f.requires_thorns.iter().map(|t| Requirement::Thorn(t.clone())));
        v.extend(f.requires_functions.iter().map(|x| Requirement::Function(x.clone())));
        v.dedup();
        v
    }

    fn meets(&self, thorn: &str, f: &ThornFacts, r: &Requirement) -> bool {
        let eq = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
        match r {
            Requirement::Implementation(i) => f.implements.as_deref().is_some_and(|x| eq(x, i)),
            Requirement::Capability(c) => f.provides.iter().any(|p| eq(p, c)),
            Requirement::Thorn(t) => eq(thorn, t),
            Requirement::Function(x) => f.provides_functions.iter().any(|p| eq(p, x)),
        }
    }
}

/// `implementation:hyperslab`: how a case's `providers` names a requirement.
fn requirement_key(r: &Requirement) -> String {
    let (kind, name) = match r {
        Requirement::Implementation(n) => ("implementation", n),
        Requirement::Capability(n) => ("capability", n),
        Requirement::Thorn(n) => ("thorn", n),
        Requirement::Function(n) => ("function", n),
    };
    format!("{kind}:{}", name.to_ascii_lowercase())
}

#[derive(Default)]
struct FixtureFacts {
    releases: BTreeMap<PkgId, Vec<Release>>,
    snapshots: BTreeMap<(PkgId, String), Snapshot>,
    providers: BTreeMap<String, Vec<Provider>>,
    /// Each `load` call, in order.
    waves: Vec<Vec<PkgId>>,
}

impl Facts for FixtureFacts {
    fn load(&mut self, ids: &[PkgId]) -> Res<()> {
        assert!(ids.iter().all(|id| !id.is_repo()), "a pseudo-package was asked for: {ids:?}");
        self.waves.push(ids.to_vec());
        Ok(())
    }

    fn releases(&self, id: &PkgId) -> Option<&[Release]> {
        self.releases.get(id).map(Vec::as_slice)
    }

    fn snapshot(&mut self, id: &PkgId, at: &str) -> Res<Snapshot> {
        let found = self.snapshots.iter().find(|((i, a), _)| i == id && (a == at || a.starts_with(at)));
        found.map(|(_, s)| s.clone()).ok_or_else(|| anyhow!("no snapshot of {id} at {at} in the fixture"))
    }

    fn providers(&mut self, r: &Requirement) -> Res<Vec<Provider>> {
        Ok(self.providers.get(&requirement_key(r)).cloned().unwrap_or_default())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    #[allow(dead_code)]
    about: String,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    specs: Vec<String>,
    #[serde(default)]
    present: Vec<PresentSpec>,
    #[serde(default)]
    options: OptionsSpec,
    #[serde(default)]
    ignore: Vec<String>,
    #[serde(default)]
    overrides: Vec<OverrideSpec>,
    #[serde(default)]
    providers: BTreeMap<String, Vec<ProviderSpec>>,
    #[serde(default)]
    remove: Option<RemoveSpec>,
    expect: Expect,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PresentSpec {
    package: String,
    /// The served version it is at (looked up in `versions/`)...
    #[serde(default)]
    version: Option<String>,
    /// ...or a commit no version names (`At::Unknown`), with its thorns and claims given here.
    #[serde(default)]
    unknown: Option<String>,
    #[serde(default)]
    thorns: Vec<ThornInfo>,
    #[serde(default)]
    claims: Vec<ClaimInfo>,
    #[serde(default)]
    flesh: bool,
    /// The base repo it is (absent: an `[[added]]` package).
    #[serde(default)]
    base: Option<String>,
    enabled: Vec<String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct OptionsSpec {
    #[serde(default)]
    upgrade: bool,
    #[serde(default)]
    upgrade_all: bool,
    #[serde(default)]
    no_deps: bool,
    #[serde(default)]
    replace: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
enum OverrideSpec {
    Drop(String),
    Add { package: String, claim: ClaimInfo },
    Needs { package: String, target: String, thorns: Vec<String> },
    Satisfies { by: String, target: String },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderSpec {
    package: String,
    version: String,
    arrangement: String,
    thorn: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct RemoveSpec {
    targets: Vec<String>,
    #[serde(default)]
    added_for: BTreeMap<String, AddedForSpec>,
    #[serde(default)]
    no_deps: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct AddedForSpec {
    requested: bool,
    #[serde(default)]
    by: Vec<String>,
    #[serde(default)]
    choice: Option<String>,
    /// `"current"`: the commit the package is at; anything else is taken as written (stale).
    #[serde(default)]
    for_commit: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Expect {
    /// The rendered plan below its header line, spaces collapsed.
    #[serde(default)]
    plan: Option<Vec<String>>,
    #[serde(default)]
    choices: Option<Vec<String>>,
    #[serde(default)]
    records: Option<Vec<String>>,
    #[serde(default)]
    libraries: Option<Vec<String>>,
    #[serde(default)]
    untrusted: Option<Vec<String>>,
    #[serde(default)]
    pending: Option<Vec<String>>,
    /// The refusal's variant.
    #[serde(default)]
    refusal: Option<String>,
    /// Text the refusal's message must contain (spaces collapsed).
    #[serde(default)]
    message: Vec<String>,
    #[serde(default)]
    removed: Option<Vec<String>>,
    #[serde(default)]
    orphans: Option<Vec<String>>,
}

fn cases_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/cactarium")
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn read_json(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn json_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut v: Vec<PathBuf> =
        rd.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
    v.sort();
    v
}

/// A served version, with the review state the crate does not model yet.
fn served(id: &PkgId, v: &Value) -> Release {
    let pending = v.get("review").and_then(Value::as_str) == Some("pending");
    let pv: PackageVersion = serde_json::from_value(v.clone()).expect("a PackageVersion");
    Release::from_served(id, &pv, pending).expect("a served release")
}

struct Loaded {
    case: Case,
    facts: FixtureFacts,
    present: Vec<Present>,
    rules: Rules,
}

fn load(dir: &Path) -> Loaded {
    let case: Case = serde_json::from_value(read_json(&dir.join("case.json")))
        .unwrap_or_else(|e| panic!("{}/case.json: {e}", dir.display()));
    let mut facts = FixtureFacts::default();
    for path in json_files(&dir.join("versions")) {
        let v = read_json(&path);
        // The real shape, whole: it must parse as the crate's type.
        let _: cactup_ecosystem_common::api::PackageVersions =
            serde_json::from_value(v.clone()).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let id = PkgId::parse(v["slug"].as_str().expect("a slug")).unwrap();
        let releases = v["versions"].as_array().expect("versions").iter().map(|x| served(&id, x)).collect();
        facts.releases.insert(id, releases);
    }
    for path in json_files(&dir.join("snapshots")) {
        let v = read_json(&path);
        let id = PkgId::parse(v["slug"].as_str().expect("a slug")).unwrap();
        let release = served(&id, &v["served"]);
        let untrusted = v["untrusted"].as_bool().unwrap_or(false);
        facts.snapshots.insert((id, v["at"].as_str().unwrap().to_owned()), Snapshot { release, untrusted });
    }
    for (key, list) in &case.providers {
        let providers = list
            .iter()
            .map(|p| Provider {
                package: PkgId::parse(&p.package).unwrap(),
                version: p.version.clone(),
                arrangement: p.arrangement.clone(),
                thorn: p.thorn.clone(),
            })
            .collect();
        facts.providers.insert(key.clone(), providers);
    }
    let mut present = Vec::new();
    for p in &case.present {
        let id = PkgId::parse(&p.package).unwrap();
        let release = match (&p.version, &p.unknown) {
            (Some(v), None) => facts.releases[&id]
                .iter()
                .find(|r| r.at.version().is_some_and(|x| x.to_string() == *v))
                .unwrap_or_else(|| panic!("{}: no {id}@{v} in versions/", dir.display()))
                .clone(),
            (None, Some(commit)) => Release {
                id: id.clone(),
                at: At::Unknown(commit.clone()),
                commit: commit.clone(),
                url: String::new(),
                flesh: p.flesh,
                library: false,
                yanked: false,
                pending: false,
                usable: true,
                thorns: p.thorns.clone(),
                claims: p.claims.clone(),
            },
            _ => panic!("{}: {id} needs one of version and unknown", dir.display()),
        };
        let origin = match &p.base {
            Some(repo) => Origin::Base { repo: repo.clone() },
            None => Origin::Added,
        };
        present.push(Present { release, enabled: p.enabled.iter().cloned().collect(), origin });
    }
    let mut rules = Rules::default();
    for i in &case.ignore {
        rules.ignore.push(i.parse().unwrap());
    }
    for o in &case.overrides {
        rules.overrides.push(match o {
            OverrideSpec::Drop(p) => Override::Drop(p.parse().unwrap()),
            OverrideSpec::Add { package, claim } => Override::Add { package: PkgId::parse(package).unwrap(), claim: claim.clone() },
            OverrideSpec::Needs { package, target, thorns } => Override::Needs {
                package: PkgId::parse(package).unwrap(),
                target: PkgId::parse(target).unwrap(),
                thorns: thorns.clone(),
            },
            OverrideSpec::Satisfies { by, target } => {
                Override::Satisfies { by: PkgId::parse(by).unwrap(), target: PkgId::parse(target).unwrap() }
            }
        });
    }
    Loaded { case, facts, present, rules }
}

fn variant(r: &Refusal) -> String {
    let debug = format!("{r:?}");
    debug.split([' ', '(', '{']).next().unwrap_or_default().to_owned()
}

fn compare(what: &str, want: &Option<Vec<String>>, got: Vec<String>, errors: &mut Vec<String>) {
    let Some(want) = want else { return };
    let want: Vec<String> = want.iter().map(|s| collapse(s)).collect();
    let got: Vec<String> = got.iter().map(|s| collapse(s)).collect();
    if want != got {
        errors.push(format!("{what}:\n    want {want:#?}\n    got  {got:#?}"));
    }
}

fn check_refusal(expect: &Expect, refusal: Option<&Refusal>, other: Option<String>, errors: &mut Vec<String>) {
    match (&expect.refusal, refusal) {
        (Some(want), Some(r)) => {
            let text = collapse(&r.to_string());
            if &variant(r) != want {
                errors.push(format!("refused with {} ({text}), not {want}", variant(r)));
            }
            for m in &expect.message {
                if !text.contains(&collapse(m)) {
                    errors.push(format!("the message lacks \"{m}\": {text}"));
                }
            }
            if text.contains('§') {
                errors.push(format!("a spec ref in user-visible text: {text}"));
            }
        }
        (Some(want), None) => errors.push(format!("expected a {want} refusal, got {}", other.unwrap_or("a plan".into()))),
        (None, Some(r)) => errors.push(format!("refused: {r}")),
        (None, None) => {
            if let Some(e) = other {
                errors.push(format!("failed: {e}"));
            }
        }
    }
}

fn record_text(r: &Record) -> String {
    let mut s = format!("{} {} by [{}]", r.id, if r.requested { "requested" } else { "brought in" }, r.by.join(", "));
    if let Some(c) = &r.choice {
        s.push_str(&format!(" choice: {c}"));
    }
    s
}

fn library_text(l: &Library) -> String {
    let places: Vec<String> = l.placements.iter().map(|p| format!("into {} by {}", p.into, p.by)).collect();
    format!("{} {}", l.release.label(), places.join("; "))
}

/// Runs one case; the problems found, empty when it passes.
fn run(dir: &Path) -> Vec<String> {
    let Loaded { case, mut facts, present, rules } = load(dir);
    let mut errors = Vec::new();
    if let Some(rm) = &case.remove {
        let targets: Vec<PkgId> = rm.targets.iter().map(|t| PkgId::parse(t).unwrap()).collect();
        let mut added_for = BTreeMap::new();
        for (id, a) in &rm.added_for {
            let id = PkgId::parse(id).unwrap();
            let commit = present.iter().find(|p| p.release.id == id).map(|p| p.release.commit.clone()).unwrap_or_default();
            let for_commit = a.for_commit.as_ref().map(|c| if c == "current" { commit.clone() } else { c.clone() });
            added_for.insert(id, AddedFor { requested: a.requested, by: a.by.clone(), choice: a.choice.clone(), for_commit });
        }
        match plan_remove(&present, &added_for, &targets, &rules, rm.no_deps) {
            Ok(plan) => {
                check_refusal(&case.expect, None, None, &mut errors);
                compare("removed", &case.expect.removed, plan.removed.iter().map(ToString::to_string).collect(), &mut errors);
                let orphans = plan.orphans.iter().map(|o| format!("{}: {}", o.label, o.why())).collect();
                compare("orphans", &case.expect.orphans, orphans, &mut errors);
            }
            Err(r) => check_refusal(&case.expect, Some(&r), None, &mut errors),
        }
        return errors;
    }
    let specs: Vec<NamedSpec> = case.specs.iter().map(|s| NamedSpec::parse(s).unwrap()).collect();
    let o = &case.options;
    let options = Options { upgrade: o.upgrade, upgrade_all: o.upgrade_all, no_deps: o.no_deps, replace: o.replace };
    let request = Request { specs: &specs, present: &present, rules: &rules, options };
    match resolve(&request, &mut facts, &ServerRules) {
        Ok(plan) => {
            check_refusal(&case.expect, None, None, &mut errors);
            let command = case.command.clone().unwrap_or_else(|| format!("cactup add {}", case.specs.join(" ")));
            let rendered = plan.render(&command);
            let mut lines = rendered.lines();
            if lines.next() != Some(format!("Plan for {command}:").as_str()) {
                errors.push(format!("the plan's header is not \"Plan for {command}:\": {rendered}"));
            }
            compare("plan", &case.expect.plan, lines.map(str::to_owned).collect(), &mut errors);
            compare("choices", &case.expect.choices, plan.choices.iter().map(ToString::to_string).collect(), &mut errors);
            compare("records", &case.expect.records, plan.records.iter().map(record_text).collect(), &mut errors);
            compare("libraries", &case.expect.libraries, plan.libraries.iter().map(library_text).collect(), &mut errors);
            let untrusted = plan.untrusted.iter().map(|u| format!("{}#{}", u.id, u.commit)).collect();
            compare("untrusted", &case.expect.untrusted, untrusted, &mut errors);
            compare("pending", &case.expect.pending, plan.pending.clone(), &mut errors);
        }
        Err(e) => match e.downcast_ref::<Refusal>() {
            Some(r) => check_refusal(&case.expect, Some(r), None, &mut errors),
            None => check_refusal(&case.expect, None, Some(format!("{e:#}")), &mut errors),
        },
    }
    errors
}

fn case_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(cases_dir())
        .expect("fixtures/cactarium")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.join("case.json").is_file())
        .collect();
    dirs.sort();
    dirs
}

#[test]
fn resolver_tables() {
    let dirs = case_dirs();
    assert!(dirs.len() >= 40, "only {} cases under fixtures/cactarium", dirs.len());
    let mut failed = Vec::new();
    for dir in &dirs {
        let errors = run(dir);
        if !errors.is_empty() {
            let name = dir.file_name().unwrap().to_string_lossy().into_owned();
            failed.push(format!("{name}:\n  {}", errors.join("\n  ")));
        }
    }
    assert!(failed.is_empty(), "{} of {} cases failed:\n\n{}", failed.len(), dirs.len(), failed.join("\n\n"));
}

/// The same request with the present packages and the specs listed in other orders gives the
/// same plan: nothing depends on the order a map or the caller produced.
#[test]
fn plans_do_not_depend_on_input_order() {
    for name in ["any-of-order", "transitive-closure", "any-of-first-without-conflict", "upgrade-moves-touched"] {
        let dir = cases_dir().join(name);
        let render = |reverse: bool| {
            let Loaded { case, mut facts, mut present, rules } = load(&dir);
            let mut specs: Vec<NamedSpec> = case.specs.iter().map(|s| NamedSpec::parse(s).unwrap()).collect();
            if reverse {
                present.reverse();
                specs.reverse();
            }
            let o = &case.options;
            let options = Options { upgrade: o.upgrade, upgrade_all: o.upgrade_all, no_deps: o.no_deps, replace: o.replace };
            let request = Request { specs: &specs, present: &present, rules: &rules, options };
            let plan = resolve(&request, &mut facts, &ServerRules).unwrap();
            let choices: Vec<String> = plan.choices.iter().map(ToString::to_string).collect();
            // Specs in another order are steps in another order; the decisions are the same.
            let mut lines: Vec<String> = plan.render("x").lines().map(str::to_owned).collect();
            lines.sort();
            (lines, choices)
        };
        assert_eq!(render(false), render(true), "{name}");
    }
}

/// Versions are asked for in waves: the named packages, then each set of targets that comes
/// due together (both alternatives of an any-of in one request).
#[test]
fn versions_are_loaded_in_waves() {
    let dir = cases_dir().join("any-of-first-listed");
    let Loaded { case, mut facts, present, rules } = load(&dir);
    let specs: Vec<NamedSpec> = case.specs.iter().map(|s| NamedSpec::parse(s).unwrap()).collect();
    let request = Request { specs: &specs, present: &present, rules: &rules, options: Options::default() };
    resolve(&request, &mut facts, &ServerRules).unwrap();
    let waves: Vec<Vec<String>> = facts.waves.iter().map(|w| w.iter().map(ToString::to_string).collect()).collect();
    assert_eq!(waves, [vec!["max/z4c"], vec!["et/cactuspugh", "et/carpet"]]);
}

fn plan_of(name: &str) -> Plan {
    let Loaded { case, mut facts, present, rules } = load(&cases_dir().join(name));
    let specs: Vec<NamedSpec> = case.specs.iter().map(|s| NamedSpec::parse(s).unwrap()).collect();
    let o = &case.options;
    let options = Options { upgrade: o.upgrade, upgrade_all: o.upgrade_all, no_deps: o.no_deps, replace: o.replace };
    let request = Request { specs: &specs, present: &present, rules: &rules, options };
    resolve(&request, &mut facts, &ServerRules).unwrap()
}

/// A plan of only what was named is not confirmed; anything brought in, moved or replaced is.
#[test]
fn plans_say_when_they_go_beyond_what_was_named() {
    assert!(!plan_of("fixed-when-it-fits").beyond_named());
    assert!(!plan_of("fork-stands-in-by-range").beyond_named(), "a keep line is no change");
    assert!(plan_of("transitive-closure").beyond_named());
    assert!(plan_of("same-name-replace").beyond_named(), "a replaced base repo");
    assert!(plan_of("upgrade-all").beyond_named());
    assert!(Plan::default().is_empty() && !plan_of("fixed-when-it-fits").is_empty());
}

#[test]
fn a_missing_or_stale_added_for_means_requested() {
    let c = "0123456789abcdef0123456789abcdef01234567";
    let mine = AddedFor { requested: false, for_commit: Some(c.into()), ..AddedFor::default() };
    assert!(!is_requested(Some(&mine), c));
    assert!(is_requested(None, c), "an installation from before added-for");
    assert!(is_requested(Some(&mine), "fedcba9876543210fedcba9876543210fedcba98"), "re-added by an older binary");
    let legacy = AddedFor { requested: false, for_commit: None, ..AddedFor::default() };
    assert!(is_requested(Some(&legacy), c));
}

#[test]
fn named_specs_parse() {
    let s = NamedSpec::parse("max/z4c/Z4c@^1.2").unwrap();
    assert_eq!((s.id.as_str(), s.thorns.as_deref()), ("max/z4c", Some(&["Z4c".to_owned()][..])));
    assert_eq!(s.req, Req::Range("^1.2".parse().unwrap()));
    assert_eq!(NamedSpec::parse("max/z4c#abcdef1").unwrap().req, Req::Commit("abcdef1".into()));
    assert_eq!(NamedSpec::parse("max/z4c@head").unwrap().req, Req::Tip("head".into()));
    // `@<hex>` is a commit only when it names no range or track, as the crate reads it.
    assert_eq!(NamedSpec::parse("max/z4c@8a1b2c3d").unwrap().req, Req::Commit("8a1b2c3d".into()));
    assert!(matches!(NamedSpec::parse("max/z4c@1234567").unwrap().req, Req::Range(_)));
    assert!(matches!(NamedSpec::parse("max/z4c@abcdef12").unwrap().req, Req::Range(_)), "a track");
    assert!(NamedSpec::parse("max/z4c").unwrap().thorns.is_none());
    assert!(NamedSpec::parse("Max/z4c").is_err());
}

#[test]
fn claim_patterns_round_trip() {
    for s in ["max/z4c", "max/z4c:dependency:et/carpet", "et/a:incompatible-with:et/b", "max/cactus-mp:fork-of:et/cactus"] {
        assert_eq!(s.parse::<ClaimPattern>().unwrap().to_string(), s);
    }
    assert!("max/z4c:depends:et/carpet".parse::<ClaimPattern>().is_err());
    assert!("max/z4c:dependency".parse::<ClaimPattern>().is_err());
}
