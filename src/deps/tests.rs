//! Table tests: every directory under `fixtures/cactarium/` holding a `case.json` is one case.
//!
//! - `fixtures/cactarium/pool/*.json`: `GET /packages/{o}/{n}/versions` answers
//!   (`PackageVersions`), as the Cactarium serves them, shared by the cases; `pool/snapshots/`
//!   holds `GET …/versions/{commit|head|master}` answers as `{"slug", "at", "untrusted",
//!   "served"}`. `fixtures/cactarium/generate.py` writes the pool. A version may carry
//!   `"review": "pending"` (S6, plan §6), read as the crate's `VersionInfo.review`.
//! - `<case>/case.json`, edited by hand: the pool files it uses, the request (specs, present
//!   packages, options, ignores, overrides, the providers `GET /providers` answers with) or a
//!   removal, and what must come out.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cactup_ecosystem_common::api::{ClaimInfo, PackageVersion, ThornInfo};
use cactup_ecosystem_common::ccl::ThornFacts;
use cactup_ecosystem_common::version::TrackedVersion;
use serde::Deserialize;
use serde_json::Value;

use super::*;

/// et/carpetx 2026.5.1's commit in the pool (the sha1 of "et/carpetx@2026.5.1").
const C551: &str = "ba20bf7e830f893466fceda170ebf5b844c342ba";

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
    /// The progress ticks heard.
    ticks: Vec<usize>,
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

    fn snapshot(&mut self, id: &PkgId, at: &str) -> Res<Option<Snapshot>> {
        let found = self.snapshots.iter().find(|((i, a), _)| i == id && (a == at || a.starts_with(at)));
        Ok(found.map(|(_, s)| s.clone()))
    }

    fn providers(&mut self, r: &Requirement) -> Res<Vec<Provider>> {
        Ok(self.providers.get(&requirement_key(r)).cloned().unwrap_or_default())
    }

    fn tick(&mut self, steps: usize) {
        self.ticks.push(steps);
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    #[allow(dead_code)]
    about: String,
    /// Pool files (`pool/<name>.json`) whose packages the Cactarium serves.
    packages: Vec<String>,
    #[serde(default)]
    snapshots: Vec<String>,
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
    /// The served version it is at (looked up among the case's packages)...
    #[serde(default)]
    version: Option<String>,
    /// ...or a commit no version names (`At::Unknown`), with its thorns given here.
    #[serde(default)]
    unknown: Option<String>,
    #[serde(default)]
    thorns: Vec<ThornSpec>,
    /// The base repo it is (absent: an `[[added]]` package).
    #[serde(default)]
    base: Option<String>,
    enabled: Vec<String>,
    #[serde(default)]
    libraries: Vec<LibrarySpec>,
}

/// A present thorn: its name (default facts), or its name with the facts that differ.
#[derive(Deserialize)]
#[serde(untagged)]
enum ThornSpec {
    Name(String),
    Full {
        thorn: String,
        #[serde(default)]
        arrangement: Option<String>,
        #[serde(default)]
        facts: serde_json::Map<String, Value>,
    },
}

impl ThornSpec {
    fn info(&self) -> ThornInfo {
        let (thorn, arrangement, given) = match self {
            ThornSpec::Name(n) => (n.clone(), None, serde_json::Map::new()),
            ThornSpec::Full { thorn, arrangement, facts } => (thorn.clone(), arrangement.clone(), facts.clone()),
        };
        let mut facts = serde_json::json!({
            "implements": thorn, "inherits": [], "friends": [], "requires-functions": [],
            "uses-functions": [], "provides-functions": [], "uses-includes": [], "requires": [],
            "requires-thorns": [], "optional": [], "provides": []
        });
        for (k, v) in given {
            assert!(facts.get(&k).is_some(), "{k} is no fact");
            facts[k] = v;
        }
        ThornInfo {
            arrangement: arrangement.unwrap_or_else(|| "Base".into()),
            thorn,
            repo_path: "$1/$2".into(),
            facts: serde_json::from_value(facts).expect("facts"),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LibrarySpec {
    package: String,
    commit: String,
    into: String,
    #[serde(default)]
    inside: Option<String>,
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
    choices: Vec<ChoiceKeySpec>,
    /// `"current"`: the commit the package is at; anything else is taken as written (stale).
    #[serde(default)]
    for_commit: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChoiceKeySpec {
    dependent: String,
    alternatives: Vec<String>,
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

/// A served version.
fn served(id: &PkgId, v: &Value) -> Release {
    let pv: PackageVersion = serde_json::from_value(v.clone()).expect("a PackageVersion");
    Release::from_served(id, &pv, None).expect("a served release")
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
    let pool = cases_dir().join("pool");
    let mut facts = FixtureFacts::default();
    for name in &case.packages {
        let path = pool.join(format!("{name}.json"));
        let v = read_json(&path);
        // The real shape, whole: it must parse as the crate's type.
        let _: cactup_ecosystem_common::api::PackageVersions =
            serde_json::from_value(v.clone()).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let id = PkgId::parse(v["slug"].as_str().expect("a slug")).unwrap();
        let releases = v["versions"].as_array().expect("versions").iter().map(|x| served(&id, x)).collect();
        if facts.releases.insert(id.clone(), releases).is_some() {
            panic!("{}: two pool files serve {id}", dir.display());
        }
    }
    for name in &case.snapshots {
        let v = read_json(&pool.join("snapshots").join(format!("{name}.json")));
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
                .unwrap_or_else(|| panic!("{}: no {id}@{v} among its packages", dir.display()))
                .clone(),
            (None, Some(commit)) => Release {
                id: id.clone(),
                at: At::Unknown(commit.clone()),
                commit: commit.clone(),
                tag: None,
                url: String::new(),
                contact: None,
                flesh: false,
                library: false,
                yanked: false,
                yank_reason: None,
                pending: false,
                usable: true,
                thorns: p.thorns.iter().map(ThornSpec::info).collect(),
                claims: Vec::new(),
            },
            _ => panic!("{}: {id} needs one of version and unknown", dir.display()),
        };
        let origin = match &p.base {
            Some(repo) => Origin::Base { repo: repo.clone() },
            None => Origin::Added,
        };
        let libraries = p
            .libraries
            .iter()
            .map(|l| InstalledLibrary {
                id: PkgId::parse(&l.package).unwrap(),
                commit: l.commit.clone(),
                into: l.into.clone(),
                inside: l.inside.as_deref().map(|i| PkgId::parse(i).unwrap()),
            })
            .collect();
        present.push(Present { release, enabled: p.enabled.iter().cloned().collect(), origin, libraries });
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
            // Until P4 adds the command, no refusal may name it.
            if text.contains("claims override") {
                errors.push(format!("names a command that does not exist yet: {text}"));
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
    let requested = match r.requested {
        Some(true) => "requested",
        Some(false) => "brought in",
        None => "as recorded",
    };
    let mut s = format!("{} {requested} by [{}]", r.id, r.by.join(", "));
    for c in &r.choices {
        let alts: Vec<String> = c.alternatives.iter().map(ToString::to_string).collect();
        s.push_str(&format!(" choice {}: {}", c.dependent, alts.join(", ")));
    }
    s
}

fn library_text(l: &Library) -> String {
    let places: Vec<String> = l
        .placements
        .iter()
        .map(|p| {
            let inside = p.inside.as_ref().map(|i| format!(" inside {i}")).unwrap_or_default();
            let installed = if p.new { "" } else { " (installed)" };
            format!("{} by {}{inside}{installed}", p.into, p.by)
        })
        .collect();
    format!("{}: {}", l.release.label(), places.join("; "))
}

fn untrusted_text(u: &Untrusted) -> String {
    match &u.tip {
        Some(tip) => format!("{}#{} (tip {tip})", u.id, u.commit),
        None => format!("{}#{}", u.id, u.commit),
    }
}

fn options_of(case: &Case) -> Options {
    let o = &case.options;
    Options { upgrade: o.upgrade, upgrade_all: o.upgrade_all, no_deps: o.no_deps, replace: o.replace }
}

fn specs_of(case: &Case) -> Vec<NamedSpec> {
    case.specs.iter().map(|s| NamedSpec::parse(s).unwrap()).collect()
}

/// Runs one case; the problems found, empty when it passes.
fn run(dir: &Path) -> Vec<String> {
    let Loaded { case, mut facts, present, rules } = load(dir);
    let mut errors = Vec::new();
    let command = case.command.clone().unwrap_or_else(|| format!("cactup add {}", case.specs.join(" ")));
    if let Some(rm) = &case.remove {
        let targets: Vec<PkgId> = rm.targets.iter().map(|t| PkgId::parse(t).unwrap()).collect();
        let mut added_for = BTreeMap::new();
        for (id, a) in &rm.added_for {
            let id = PkgId::parse(id).unwrap();
            let commit = present.iter().find(|p| p.release.id == id).map(|p| p.release.commit.clone()).unwrap_or_default();
            let for_commit = a.for_commit.as_ref().map(|c| if c == "current" { commit.clone() } else { c.clone() });
            let choices = a
                .choices
                .iter()
                .map(|c| ChoiceKey {
                    dependent: PkgId::parse(&c.dependent).unwrap(),
                    alternatives: c.alternatives.iter().map(|x| PkgId::parse(x).unwrap()).collect(),
                })
                .collect();
            added_for.insert(id, AddedFor { requested: a.requested, by: a.by.clone(), choices, for_commit });
        }
        match plan_remove(&present, &added_for, &targets, &rules, rm.no_deps, &[]) {
            Ok(plan) => {
                check_refusal(&case.expect, None, None, &mut errors);
                compare("removed", &case.expect.removed, plan.removed.iter().map(ToString::to_string).collect(), &mut errors);
                let orphans = plan.orphans.iter().map(|o| format!("{}: {}", o.label, o.why())).collect();
                compare("orphans", &case.expect.orphans, orphans, &mut errors);
                let rendered = plan.render("cactup remove");
                compare("plan", &case.expect.plan, rendered.lines().skip(1).map(str::to_owned).collect(), &mut errors);
            }
            Err(r) => check_refusal(&case.expect, Some(&r), None, &mut errors),
        }
        return errors;
    }
    let specs = specs_of(&case);
    let request = Request { specs: &specs, present: &present, rules: &rules, options: options_of(&case) };
    match resolve(&request, &mut facts) {
        Ok(plan) => {
            check_refusal(&case.expect, None, None, &mut errors);
            let rendered = plan.render(&command);
            let mut lines = rendered.lines();
            if lines.next() != Some(format!("Plan for {command}:").as_str()) {
                errors.push(format!("the plan's header is not \"Plan for {command}:\": {rendered}"));
            }
            compare("plan", &case.expect.plan, lines.map(str::to_owned).collect(), &mut errors);
            compare("choices", &case.expect.choices, plan.choices.iter().map(ToString::to_string).collect(), &mut errors);
            compare("records", &case.expect.records, plan.records.iter().map(record_text).collect(), &mut errors);
            compare("libraries", &case.expect.libraries, plan.libraries.iter().map(library_text).collect(), &mut errors);
            compare("untrusted", &case.expect.untrusted, plan.untrusted.iter().map(untrusted_text).collect(), &mut errors);
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
    assert!(dirs.len() >= 100, "only {} cases under fixtures/cactarium", dirs.len());
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

/// A release's claims the other way round, except any-of dependencies, whose declaration order
/// decides which is settled first: they keep their order among themselves (after the rest).
fn reverse_claims(r: &mut Release) {
    let (any_of, mut rest): (Vec<ClaimInfo>, Vec<ClaimInfo>) =
        r.claims.drain(..).partition(|c| !c.alternatives.is_empty());
    rest.reverse();
    r.claims = rest.into_iter().chain(any_of).collect();
}

/// What a resolution decided, whatever order its steps come in.
fn outcome(dir: &Path, reverse: bool) -> String {
    let Loaded { case, mut facts, mut present, rules } = load(dir);
    let mut specs = specs_of(&case);
    if reverse {
        present.reverse();
        specs.reverse();
        for p in &mut present {
            reverse_claims(&mut p.release);
        }
        for rs in facts.releases.values_mut() {
            rs.iter_mut().for_each(reverse_claims);
        }
        for s in facts.snapshots.values_mut() {
            reverse_claims(&mut s.release);
        }
    }
    let request = Request { specs: &specs, present: &present, rules: &rules, options: options_of(&case) };
    match resolve(&request, &mut facts) {
        Ok(plan) => {
            let mut lines: Vec<String> = plan.render("x").lines().map(collapse).collect();
            lines.extend(plan.choices.iter().map(|c| format!("choice {} {c}", c.dependent)));
            lines.extend(plan.records.iter().map(record_text));
            lines.extend(plan.libraries.iter().map(library_text));
            lines.sort();
            lines.join("\n")
        }
        Err(e) => match e.downcast_ref::<Refusal>() {
            Some(r) => format!("refused: {}", variant(r)),
            None => format!("failed: {e:#}"),
        },
    }
}

/// Every case gives the same decisions with its present packages, its specs and each release's
/// claims (any-of dependencies aside) listed the other way round: nothing depends on the order a
/// map, the server or the caller produced, nor on the order needs arrive in.
#[test]
fn plans_do_not_depend_on_input_order() {
    let mut differ = Vec::new();
    for dir in case_dirs() {
        if load(&dir).case.remove.is_some() {
            continue;
        }
        let (forward, reverse) = (outcome(&dir, false), outcome(&dir, true));
        if forward != reverse {
            let name = dir.file_name().unwrap().to_string_lossy().into_owned();
            differ.push(format!("{name}:\n--- forward\n{forward}\n--- reversed\n{reverse}"));
        }
    }
    assert!(differ.is_empty(), "{}", differ.join("\n\n"));
}

/// Versions are asked for in waves: the named packages, then each set of targets that comes
/// due together (both alternatives of an any-of in one request).
#[test]
fn versions_are_loaded_in_waves() {
    let Loaded { case, mut facts, present, rules } = load(&cases_dir().join("any-of-first-listed"));
    let specs = specs_of(&case);
    let request = Request { specs: &specs, present: &present, rules: &rules, options: Options::default() };
    resolve(&request, &mut facts).unwrap();
    let waves: Vec<Vec<String>> = facts.waves.iter().map(|w| w.iter().map(ToString::to_string).collect()).collect();
    assert_eq!(waves, [vec!["max/z4c"], vec!["et/cactuspugh", "et/carpet"]]);
}

fn plan_of(name: &str) -> Plan {
    let Loaded { case, mut facts, present, rules } = load(&cases_dir().join(name));
    let specs = specs_of(&case);
    let request = Request { specs: &specs, present: &present, rules: &rules, options: options_of(&case) };
    resolve(&request, &mut facts).unwrap()
}

/// A plan of only what was named is not confirmed; anything brought in, moved or replaced is.
#[test]
fn plans_say_when_they_go_beyond_what_was_named() {
    assert!(!plan_of("fixed-when-it-fits").beyond_named());
    assert!(!plan_of("fork-stands-in-by-range").beyond_named(), "a keep line is no change");
    assert!(!plan_of("named-present-readd").beyond_named(), "a re-add of what was named");
    assert!(!plan_of("named-base-add-thorns").beyond_named());
    assert!(!plan_of("same-name-replace").beyond_named(), "--replace on a named package");
    assert!(!plan_of("named-base-upgrade").beyond_named(), "a named base package replacing its repo");
    assert!(plan_of("transitive-closure").beyond_named());
    assert!(plan_of("flesh-replace").beyond_named(), "a flesh a dependency replaces");
    assert!(plan_of("upgrade-all").beyond_named());
    assert!(Plan::default().is_empty() && !plan_of("fixed-when-it-fits").is_empty());
    assert!(plan_of("named-base-already").is_empty());
}

#[test]
fn plan_lines_say_what_they_are() {
    let lines = plan_of("pending-when-nothing-approved-fits").lines("cactup add max/z4c");
    let kinds: Vec<LineKind> = lines.iter().map(|(k, _)| *k).collect();
    assert_eq!(kinds, [LineKind::Header, LineKind::Step, LineKind::Step, LineKind::Pending]);
    let lines = plan_of("flesh-replace").lines("x");
    assert!(lines.iter().any(|(k, _)| *k == LineKind::Rebuild));
    let lines = plan_of("unknown-meets-range-with-note").lines("x");
    assert_eq!(lines.last().map(|(k, _)| *k), Some(LineKind::Note));
}

/// What a record writes over an existing `added-for` entry.
#[test]
fn records_merge_into_added_for() {
    let (old_commit, new_commit) = ("1".repeat(40), "2".repeat(40));
    let key = ChoiceKey { dependent: PkgId::parse("max/z4c").unwrap(), alternatives: vec![] };
    let record = |requested| Record {
        id: PkgId::parse("et/a").unwrap(),
        commit: new_commit.clone(),
        requested,
        by: vec!["max/b@1.0.0".into()],
        choices: vec![key.clone()],
    };
    let brought = AddedFor {
        requested: false,
        by: vec!["max/z4c@1.2.0".into()],
        choices: vec![],
        for_commit: Some(old_commit.clone()),
    };
    let merged = record(None).merge(Some(&brought), Some(&old_commit));
    assert!(!merged.requested, "still brought in");
    assert_eq!(merged.by, ["max/z4c@1.2.0", "max/b@1.0.0"]);
    assert_eq!(merged.choices, [key.clone()]);
    assert_eq!(merged.for_commit.as_deref(), Some(new_commit.as_str()), "written for the new commit");
    assert!(record(None).merge(None, Some(&old_commit)).requested, "no entry: requested");
    let stale = AddedFor { for_commit: Some("3".repeat(40)), ..brought.clone() };
    assert!(record(None).merge(Some(&stale), Some(&old_commit)).requested, "a stale entry: requested");
    assert!(record(Some(true)).merge(Some(&brought), Some(&old_commit)).requested, "named now");
    assert!(!record(Some(false)).merge(None, None).requested, "brought in now");
    // A package brought in now starts afresh: a leftover entry passes nothing on.
    let fresh = record(Some(false)).merge(Some(&brought), Some(&old_commit));
    assert_eq!((fresh.requested, fresh.by.as_slice()), (false, &["max/b@1.0.0".to_owned()][..]));
}

/// One package present twice is the caller's mistake, refused rather than half-read.
#[test]
fn a_package_present_twice_is_refused() {
    let Loaded { mut facts, mut present, rules, .. } = load(&cases_dir().join("fixed-when-it-fits"));
    present.push(present[0].clone());
    let request = Request { specs: &[], present: &present, rules: &rules, options: Options::default() };
    let e = resolve(&request, &mut facts).unwrap_err();
    assert!(e.downcast_ref::<Refusal>().is_none() && e.to_string().contains("in the installation twice"), "{e}");
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

/// Data from the server that would make a slice or a match go wrong is refused, never a panic.
#[test]
fn bad_server_data_is_refused() {
    assert_eq!(short("abc"), "abc");
    let odd = "aéééééééé";
    assert!(odd.starts_with(short(odd)), "no slicing inside a character");
    assert!(Req::from_claim(None, Some("")).is_err(), "an empty sha");
    assert!(Req::from_claim(None, Some("xyz1234")).is_err());
    assert!(Req::from_claim(None, Some("ABCDEF1")).is_ok_and(|r| r == Req::Commit("abcdef1".into())));
    let empty = Req::Commit(String::new());
    let mut v: Value = read_json(&cases_dir().join("pool/et--carpetx.json"))["versions"][0].clone();
    let id = PkgId::parse("et/carpetx").unwrap();
    assert_eq!(empty.fit(&served(&id, &v)), Fit::No, "an empty commit matches nothing");
    for bad in [Value::Null, "".into(), "zz".into(), "abc".into()] {
        v["commit"] = bad;
        let pv: PackageVersion = serde_json::from_value(v.clone()).unwrap();
        assert!(Release::from_served(&id, &pv, None).is_err(), "{}", v["commit"]);
    }
    // A fork-of claim whose sha or range does not read is no claim (never a panic later).
    let fork = read_json(&cases_dir().join("pool/max--carpetx-mp--bad-sha.json"))["versions"][0].clone();
    let fork = served(&PkgId::parse("max/carpetx-mp").unwrap(), &fork);
    assert!(fork_claims(&fork, &id).is_empty() && !forks(&fork, &id));
    let mut bad_range = fork.clone();
    bad_range.claims[0].sha = None;
    bad_range.claims[0].range = Some("not a range".into());
    // The crate keeps what follows `@` raw: still a fork, standing in for no version.
    let dep: cactup_ecosystem_common::slug::Slug = "et/carpetx@^2026.5".parse().unwrap();
    let published = [(TrackedVersion::from_tag("v2026.5.1").unwrap(), Some(C551.to_owned()))];
    assert!(fork_claims(&bad_range, &id).iter().all(|f| !f.stands_in_for(&dep, &published)));
}

#[test]
fn tips_and_commits_are_read_from_the_version() {
    let id = PkgId::parse("et/carpetx").unwrap();
    let v = read_json(&cases_dir().join("pool/snapshots/et--carpetx--at-head.json"));
    let r = served(&id, &v["served"]);
    assert!(matches!(&r.at, At::Tip { name, .. } if name == "head"), "{:?}", r.at);
    assert_eq!(r.label(), "et/carpetx@head");
    let v = read_json(&cases_dir().join("pool/snapshots/et--carpetx--at-4444.json"));
    assert!(matches!(served(&id, &v["served"]).at, At::Commit(_)));
    let v = read_json(&cases_dir().join("pool/et--carpetx.json"));
    let r = served(&id, &v["versions"][0]);
    assert_eq!((r.tag.as_deref(), r.url.as_str(), r.contact.as_deref()), (Some("v2026.5.1"), "https://github.com/et/carpetx.git", None));
    let pv: PackageVersion = serde_json::from_value(v["versions"][0].clone()).unwrap();
    let r = Release::from_served(&id, &pv, Some("help@example.org")).unwrap();
    assert_eq!(r.contact.as_deref(), Some("help@example.org"), "the package's contact");
    assert!(!r.pending);
    let pending = read_json(&cases_dir().join("pool/et--c--pending.json"))["versions"][0].clone();
    let pv: PackageVersion = serde_json::from_value(pending).unwrap();
    assert!(Release::from_served(&PkgId::parse("et/c").unwrap(), &pv, None).unwrap().pending, "review: pending");
    assert_eq!(PkgId::repo("carpetx").as_str(), "repo:carpetx");
}

/// A tip that moves shows the commits, not "head -> head".
#[test]
fn a_moving_tip_shows_its_commits() {
    let id = PkgId::parse("et/carpetx").unwrap();
    let v = read_json(&cases_dir().join("pool/snapshots/et--carpetx--at-head.json"));
    let to = served(&id, &v["served"]);
    let from = Release { at: At::Tip { name: "head".into(), commit: "1".repeat(40) }, commit: "1".repeat(40), ..to.clone() };
    let step = Step::Upgrade { from, to, thorns: vec![], why: Why::Named, flag: None };
    let plan = Plan { steps: vec![step], ..Plan::default() };
    assert!(plan.render("x").contains("et/carpetx head (111111111111) -> head (444444444444)"), "{}", plan.render("x"));
}

/// Every pool file is some case's: the pool does not drift from what the cases use.
#[test]
fn every_pool_file_is_used() {
    let mut used: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for dir in case_dirs() {
        let case = load(&dir).case;
        used.extend(case.packages.iter().map(|p| format!("{p}.json")));
        used.extend(case.snapshots.iter().map(|s| format!("snapshots/{s}.json")));
    }
    let pool = cases_dir().join("pool");
    let mut unused = Vec::new();
    for sub in ["", "snapshots"] {
        for e in std::fs::read_dir(pool.join(sub)).unwrap().filter_map(Result::ok) {
            let name = e.file_name().to_string_lossy().into_owned();
            let rel = if sub.is_empty() { name.clone() } else { format!("{sub}/{name}") };
            if name.ends_with(".json") && !used.contains(&rel) {
                unused.push(rel);
            }
        }
    }
    unused.sort();
    assert!(unused.is_empty(), "pool files no case uses: {unused:?}");
}

/// The structured parts of a plan, which P3 executes from (no label parsing).
#[test]
fn plan_steps_are_structured() {
    let plan = plan_of("named-base-add-thorns");
    assert!(matches!(&plan.steps[..], [Step::AddThorns { id, .. }] if id.as_str() == "et/admbase"));
    let plan = plan_of("replace-subset");
    let replaced = plan.steps.iter().find_map(|s| match s {
        Step::Replace { replaced, by, thorns, flesh, explicit } => Some((replaced.clone(), by.clone(), thorns.clone(), *flesh, *explicit)),
        _ => None,
    });
    let base = Replaced::Base { repo: "carpetx".into() };
    assert_eq!(replaced, Some((base, PkgId::parse("max/carpetx-mp").unwrap(), vec!["CarpetX".to_owned()], false, true)));
    let plan = plan_of("library-installed-held");
    let p = &plan.libraries[0].placements;
    assert_eq!((p[0].by_label.as_str(), p[0].new, p[1].by_label.as_str(), p[1].new), ("max/fuka-gw@1.0.0", true, "max/kadath@1.0.0", false));
    let plan = plan_of("any-of-first-listed");
    let c: &Choice = &plan.choices[0];
    assert_eq!((c.claim, c.dependent_label.as_str(), c.chosen.as_str()), (0, "max/z4c@1.2.0", "et/carpet"));
    let kind = ClaimKind::Dependency;
    assert_eq!(kind.as_str(), "dependency");
}

/// A refusal's parts, for a caller that wants more than the text.
#[test]
fn refusals_carry_their_constraints() {
    let Loaded { case, mut facts, present, rules } = load(&cases_dir().join("unclaimed-one-provider"));
    let specs = specs_of(&case);
    let request = Request { specs: &specs, present: &present, rules: &rules, options: options_of(&case) };
    let e = resolve(&request, &mut facts).unwrap_err();
    let Some(Refusal::Unclaimed(unmet)) = e.downcast_ref::<Refusal>() else { panic!("{e}") };
    let u: &Unmet = &unmet[0];
    assert_eq!((u.thorn.as_str(), u.package.as_str(), u.providers.len()), ("Z4c", "max/z4c", 1));
    let Loaded { case, mut facts, present, rules } = load(&cases_dir().join("no-release-fits"));
    let specs = specs_of(&case);
    let request = Request { specs: &specs, present: &present, rules: &rules, options: options_of(&case) };
    let e = resolve(&request, &mut facts).unwrap_err();
    let Some(Refusal::NoReleaseFits { constraints, .. }) = e.downcast_ref::<Refusal>() else { panic!("{e}") };
    let ignores: Vec<Option<&str>> = constraints.iter().map(|l: &Live| l.ignore.as_deref()).collect();
    assert_eq!(ignores, [Some("max/grhydro:dependency:et/carpetx"), Some("max/z4c:dependency:et/carpetx")]);
}

#[test]
fn remove_plans_render() {
    let plan = RemovePlan {
        removed: vec![PkgId::parse("max/z4c").unwrap()],
        removed_labels: vec!["max/z4c@1.2.0".into()],
        orphans: vec![Orphan { id: PkgId::parse("et/a").unwrap(), label: "et/a@1.0.0".into(), by: vec!["max/z4c@1.2.0".into()] }],
    };
    assert!(plan.beyond_named());
    let lines = plan.lines("cactup remove max/z4c");
    assert_eq!(lines[0], (LineKind::Header, "Plan for cactup remove max/z4c:".to_owned()));
    assert_eq!(collapse(&lines[2].1), "remove et/a@1.0.0 brought in by max/z4c@1.2.0, nothing else needs it");
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

/// Every package id a case names (pseudo-packages aside), sorted.
fn case_ids(facts: &FixtureFacts, present: &[Present], specs: &[NamedSpec], rules: &Rules) -> Vec<PkgId> {
    let mut ids: std::collections::BTreeSet<PkgId> = std::collections::BTreeSet::new();
    let mut add = |s: &str| {
        if let Ok(id) = PkgId::parse(s)
            && !id.is_repo()
        {
            ids.insert(id);
        }
    };
    let releases = facts.releases.values().flatten().chain(facts.snapshots.values().map(|s| &s.release));
    for r in releases.chain(present.iter().map(|p| &p.release)) {
        add(r.id.as_str());
        for c in &r.claims {
            add(&c.target);
            c.alternatives.iter().for_each(|a| add(&a.target));
        }
    }
    present.iter().flat_map(|p| &p.libraries).for_each(|l| add(l.id.as_str()));
    specs.iter().for_each(|s| add(s.id.as_str()));
    facts.providers.values().flatten().for_each(|p| add(p.package.as_str()));
    for o in &rules.overrides {
        match o {
            Override::Drop(p) => add(p.package.as_str()),
            Override::Add { package, claim } => {
                add(package.as_str());
                add(&claim.target);
            }
            Override::Needs { package, target, .. } | Override::Satisfies { by: package, target } => {
                add(package.as_str());
                add(target.as_str());
            }
        }
    }
    rules.ignore.iter().for_each(|p| add(p.package.as_str()));
    ids.into_iter().collect()
}

/// A consistent rename of a case's ids, and its inverse.
struct Renaming {
    to: BTreeMap<PkgId, PkgId>,
    back: BTreeMap<PkgId, PkgId>,
}

impl Renaming {
    fn new(ids: &[PkgId], f: impl Fn(usize, &PkgId) -> PkgId) -> Renaming {
        let to: BTreeMap<PkgId, PkgId> = ids.iter().enumerate().map(|(k, id)| (id.clone(), f(k, id))).collect();
        let back = to.iter().map(|(a, b)| (b.clone(), a.clone())).collect();
        Renaming { to, back }
    }

    fn id(&self, id: &PkgId) -> PkgId {
        self.to.get(id).cloned().unwrap_or_else(|| id.clone())
    }

    fn back(&self, id: &PkgId) -> PkgId {
        self.back.get(id).cloned().unwrap_or_else(|| id.clone())
    }

    fn str(&self, s: &str) -> String {
        PkgId::parse(s).map(|id| self.id(&id).to_string()).unwrap_or_else(|_| s.to_owned())
    }

    fn release(&self, r: &mut Release) {
        r.id = self.id(&r.id);
        for c in &mut r.claims {
            c.target = self.str(&c.target);
            for a in &mut c.alternatives {
                a.target = self.str(&a.target);
            }
        }
    }

    fn pattern(&self, p: &ClaimPattern) -> ClaimPattern {
        ClaimPattern { package: self.id(&p.package), kind: p.kind.as_ref().map(|(k, t)| (*k, self.id(t))) }
    }
}

/// The decisions of a plan, ids put back through `back`, whatever order they come in.
fn decisions(plan: &Plan, back: &dyn Fn(&PkgId) -> PkgId) -> std::collections::BTreeSet<String> {
    let at = |r: &Release| match &r.at {
        At::Version(v) => v.to_string(),
        At::Commit(c) | At::Unknown(c) => c.clone(),
        At::Tip { name, commit } => format!("{name} {commit}"),
    };
    let sorted = |v: &[String]| {
        let mut v = v.to_vec();
        v.sort();
        v.join(",")
    };
    let mut out = std::collections::BTreeSet::new();
    for s in &plan.steps {
        out.insert(match s {
            Step::Add { release, thorns, .. } => format!("add {} {} {}", back(&release.id), at(release), sorted(thorns)),
            Step::Readd { release, thorns } => format!("readd {} {} {}", back(&release.id), at(release), sorted(thorns)),
            Step::AddThorns { id, thorns, .. } => format!("thorns {} {}", back(id), sorted(thorns)),
            Step::Upgrade { from, to, thorns, .. } => {
                format!("upgrade {} {} {} {}", back(&to.id), at(from), at(to), sorted(thorns))
            }
            Step::Replace { replaced, by, thorns, flesh, .. } => {
                let what = match replaced {
                    Replaced::Base { repo } => format!("repo {repo}"),
                    Replaced::Added { id } => back(id).to_string(),
                };
                format!("replace {what} by {} {} {flesh}", back(by), sorted(thorns))
            }
            Step::Keep { id, .. } => format!("keep {}", back(id)),
        });
    }
    for c in &plan.choices {
        out.insert(format!("choice {} {} -> {} {}", back(&c.dependent), c.claim, back(&c.chosen), c.rule));
    }
    for l in &plan.libraries {
        let mut ps: Vec<String> = l.placements.iter().map(|p| format!("{}:{}:{}", back(&p.by), p.into, p.new)).collect();
        ps.sort();
        out.insert(format!("library {} {} {}", back(&l.release.id), at(&l.release), ps.join(",")));
    }
    out.extend(plan.untrusted.iter().map(|u| format!("untrusted {} {}", back(&u.id), u.commit)));
    out
}

fn decided(request: &Request, facts: &mut FixtureFacts, back: &dyn Fn(&PkgId) -> PkgId) -> std::collections::BTreeSet<String> {
    match resolve(request, facts) {
        Ok(plan) => decisions(&plan, back),
        Err(e) => [format!("refused {}", e.downcast_ref::<Refusal>().map(variant).unwrap_or_default())].into(),
    }
}

/// Every case decides the same under three consistent renames of its ids: each name reversed (a
/// scramble), every id in the order reversed (`o/p<N-k>` for the k-th, owners included, so every
/// pair flips), and every id rotated by half. No decision depends on the order of ids (who stands
/// in for whom included). One exception, by design: any-of dependencies are settled in the order
/// of their dependents' ids (plan §2.2), and `any-of-order` shows the earlier one's choice winning.
#[test]
fn decisions_do_not_depend_on_ids() {
    let mut differ = Vec::new();
    for dir in case_dirs() {
        let Loaded { case, mut facts, present, rules } = load(&dir);
        if case.remove.is_some() || dir.ends_with("any-of-order") {
            continue;
        }
        let specs = specs_of(&case);
        let options = options_of(&case);
        let ids = case_ids(&facts, &present, &specs, &rules);
        let n = ids.len();
        let request = Request { specs: &specs, present: &present, rules: &rules, options };
        let original = decided(&request, &mut facts, &|id| id.clone());
        let renamings = [
            ("scrambled", Renaming::new(&ids, |_, id| {
                let (owner, name) = id.as_str().split_once('/').expect("owner/name");
                PkgId::parse(&format!("{owner}/{}", name.chars().rev().collect::<String>())).expect("a valid name")
            })),
            ("reversed", Renaming::new(&ids, |k, _| PkgId::parse(&format!("o/p{:04}", n - 1 - k)).unwrap())),
            ("rotated", Renaming::new(&ids, |k, _| PkgId::parse(&format!("o/p{:04}", (k + n / 2) % n)).unwrap())),
        ];
        for (how, rn) in &renamings {
            let Loaded { mut facts, mut present, rules, .. } = load(&dir);
            facts.releases = std::mem::take(&mut facts.releases)
                .into_iter()
                .map(|(id, mut rs)| {
                    rs.iter_mut().for_each(|r| rn.release(r));
                    (rn.id(&id), rs)
                })
                .collect();
            facts.snapshots = std::mem::take(&mut facts.snapshots)
                .into_iter()
                .map(|((id, at), mut s)| {
                    rn.release(&mut s.release);
                    ((rn.id(&id), at), s)
                })
                .collect();
            for ps in facts.providers.values_mut() {
                ps.iter_mut().for_each(|p| p.package = rn.id(&p.package));
            }
            for p in &mut present {
                rn.release(&mut p.release);
                p.libraries.iter_mut().for_each(|l| {
                    l.id = rn.id(&l.id);
                    l.inside = l.inside.as_ref().map(|i| rn.id(i));
                });
            }
            let specs: Vec<NamedSpec> = specs.iter().map(|s| NamedSpec { id: rn.id(&s.id), ..s.clone() }).collect();
            let rules = Rules {
                ignore: rules.ignore.iter().map(|p| rn.pattern(p)).collect(),
                overrides: rules
                    .overrides
                    .iter()
                    .map(|o| match o {
                        Override::Drop(p) => Override::Drop(rn.pattern(p)),
                        Override::Add { package, claim } => {
                            let mut claim = claim.clone();
                            claim.target = rn.str(&claim.target);
                            Override::Add { package: rn.id(package), claim }
                        }
                        Override::Needs { package, target, thorns } => {
                            Override::Needs { package: rn.id(package), target: rn.id(target), thorns: thorns.clone() }
                        }
                        Override::Satisfies { by, target } => Override::Satisfies { by: rn.id(by), target: rn.id(target) },
                    })
                    .collect(),
            };
            let request = Request { specs: &specs, present: &present, rules: &rules, options };
            let after = decided(&request, &mut facts, &|id| rn.back(id));
            if original != after {
                let name = dir.file_name().unwrap().to_string_lossy().into_owned();
                differ.push(format!("{name} ({how}):\n  original {original:?}\n  renamed  {after:?}"));
            }
        }
    }
    assert!(differ.is_empty(), "{}", differ.join("\n"));
}

/// Out of steps, the search reports the most specific refusal it found, not a bare "no
/// consistent set".
#[test]
fn running_out_reports_the_best_refusal_seen() {
    let dir = cases_dir().join("any-of-first-without-conflict");
    plan_of("any-of-first-without-conflict");
    let steps = super::resolve::STEPS.with(|s| s.get());
    let Loaded { case, mut facts, present, rules } = load(&dir);
    let specs = specs_of(&case);
    let request = Request { specs: &specs, present: &present, rules: &rules, options: options_of(&case) };
    // One step short of the plan: the branch that refused (et/carpet's thorn clash) was seen.
    super::resolve::BUDGET.with(|b| b.set(Some(steps - 1)));
    let result = resolve(&request, &mut facts);
    super::resolve::BUDGET.with(|b| b.set(None));
    let e = result.expect_err("out of steps");
    assert_eq!(e.downcast_ref::<Refusal>().map(variant).as_deref(), Some("SameThorn"), "{e}");
}

/// Facts of a realistic size for a thorn: a few inherits and functions, a dozen includes.
fn sized_facts(implements: &str, inherits: &[String]) -> ThornFacts {
    ThornFacts {
        implements: Some(implements.to_owned()),
        inherits: inherits.to_vec(),
        friends: vec![],
        requires_functions: vec![format!("{implements}_Fn")],
        uses_functions: (0..4).map(|i| format!("Use{i}_{implements}")).collect(),
        provides_functions: vec![format!("{implements}_Fn")],
        uses_includes: (0..12).map(|i| format!("{implements}_{i}.h")).collect(),
        requires: vec![],
        requires_thorns: vec![],
        optional: vec![],
        provides: vec![],
    }
}

fn synthetic_release(id: &str, version: &str, thorns: &[(String, ThornFacts)], claims: Vec<ClaimInfo>) -> Release {
    let id = PkgId::parse(id).unwrap();
    let commit = format!("{:040x}", {
        let mut h: u128 = 0;
        for b in format!("{id}@{version}").bytes() {
            h = h.wrapping_mul(131).wrapping_add(b as u128);
        }
        h
    });
    Release {
        id,
        at: At::Version(version.parse().unwrap()),
        commit: commit[commit.len() - 40..].to_owned(),
        tag: Some(format!("v{version}")),
        url: String::new(),
        contact: None,
        flesh: false,
        library: false,
        yanked: false,
        yank_reason: None,
        pending: false,
        usable: true,
        thorns: thorns
            .iter()
            .map(|(t, f)| ThornInfo { arrangement: "Arr".into(), thorn: t.clone(), repo_path: "$1/$2".into(), facts: f.clone() })
            .collect(),
        claims,
    }
}

fn dependency(target: &str, range: &str, needs: &[&str]) -> ClaimInfo {
    ClaimInfo {
        kind: "dependency".into(),
        target: target.into(),
        range: Some(range.into()),
        sha: None,
        needs: needs.iter().map(|s| s.to_string()).collect(),
        into: None,
        needed_by: vec![],
        alternatives: vec![],
        note: None,
    }
}

/// An installation of the size of a real one: 80 base repos with 400 thorns (each needing the
/// thorn before it and providing what the next one inherits), 4 added packages, and a named
/// package with 5 releases and 4 dependencies of 5 releases each; `clash` gives every release of
/// the named package (or of one dependency) a thorn a base repo has.
fn synthetic(clash: Option<&str>) -> (FixtureFacts, Vec<Present>, Vec<NamedSpec>) {
    let mut facts = FixtureFacts::default();
    let mut present = Vec::new();
    for i in 0..80 {
        let thorns: Vec<(String, ThornFacts)> = (0..5)
            .map(|j| {
                let name = format!("T{i}x{j}");
                let inherits = if i == 0 && j == 0 { vec![] } else { vec![format!("T{}x{}", if j == 0 { i - 1 } else { i }, (j + 4) % 5)] };
                (name.clone(), sized_facts(&name, &inherits))
            })
            .collect();
        let claims = if i > 0 { vec![dependency(&format!("base/pkg{}", i - 1), "^1", &[&format!("T{}x0", i - 1)])] } else { vec![] };
        let r = synthetic_release(&format!("base/pkg{i}"), "1.0.0", &thorns, claims);
        let enabled = thorns.iter().map(|(t, _)| t.clone()).collect();
        present.push(Present { release: r, enabled, origin: Origin::Base { repo: format!("pkg{i}") }, libraries: vec![] });
    }
    for k in 0..4 {
        let thorns = vec![(format!("Added{k}"), sized_facts(&format!("Added{k}"), &["T3x1".to_owned()]))];
        let id = format!("add/a{k}");
        let releases: Vec<Release> = (0..5).rev().map(|v| synthetic_release(&id, &format!("1.{v}.0"), &thorns, vec![])).collect();
        let enabled = [format!("Added{k}")].into_iter().collect();
        present.push(Present { release: releases[4].clone(), enabled, origin: Origin::Added, libraries: vec![] });
        facts.releases.insert(PkgId::parse(&id).unwrap(), releases);
    }
    let thorn = |name: &str, clashing: bool| {
        let name = if clashing { "T0x0".to_owned() } else { name.to_owned() };
        let f = sized_facts(&name, &["T10x2".to_owned(), "T20x3".to_owned()]);
        (name, f)
    };
    for d in 1..=4 {
        let id = format!("new/d{d}");
        let clashing = clash == Some(id.as_str());
        let releases =
            (0..5).rev().map(|v| synthetic_release(&id, &format!("1.{v}.0"), &[thorn(&format!("D{d}"), clashing)], vec![])).collect();
        facts.releases.insert(PkgId::parse(&id).unwrap(), releases);
    }
    let clashing = clash == Some("new/top");
    // A clashing dependency's thorn is the one asked of it.
    let asked = |d: usize| if clash == Some(format!("new/d{d}").as_str()) { "T0x0".to_owned() } else { format!("D{d}") };
    let claims: Vec<ClaimInfo> = (1..=4).map(|d| dependency(&format!("new/d{d}"), "^1", &[&asked(d)])).collect();
    let releases = (0..5)
        .rev()
        .map(|v| synthetic_release("new/top", &format!("1.{v}.0"), &[thorn("Top", clashing)], claims.clone()))
        .collect();
    facts.releases.insert(PkgId::parse("new/top").unwrap(), releases);
    (facts, present, vec![NamedSpec::parse("new/top").unwrap()])
}

fn timed(clash: Option<&str>) -> (Res<Plan>, usize, std::time::Duration) {
    timed_with(clash, Options::default())
}

fn timed_with(clash: Option<&str>, options: Options) -> (Res<Plan>, usize, std::time::Duration) {
    let (mut facts, present, specs) = synthetic(clash);
    let rules = Rules::default();
    let request = Request { specs: &specs, present: &present, rules: &rules, options };
    let start = std::time::Instant::now();
    let result = resolve(&request, &mut facts);
    (result, super::resolve::STEPS.with(|s| s.get()), start.elapsed())
}

/// A realistic installation resolves in a handful of steps.
#[test]
fn a_realistic_installation_resolves_quickly() {
    let (result, steps, time) = timed(None);
    eprintln!("realistic installation: {steps} steps, {time:?}");
    let plan = result.unwrap();
    assert_eq!(plan.steps.len(), 5, "{}", plan.render("x"));
    assert!(steps <= 10, "{steps} steps");
    assert!(time < std::time::Duration::from_secs(1), "{time:?}");
}

/// A thorn clash on every release refuses at once with the clash, in the named package or deep in
/// a dependency (the decisions it does not involve are jumped over).
#[test]
fn a_clash_on_every_release_refuses_quickly() {
    for clash in ["new/top", "new/d3"] {
        let (result, steps, time) = timed(Some(clash));
        eprintln!("clash in {clash}: {steps} steps, {time:?}");
        let e = result.expect_err("a clash");
        assert_eq!(e.downcast_ref::<Refusal>().map(variant).as_deref(), Some("SameThorn"), "{e}");
        // Linear: each release of new/top tries new/d3's five and jumps over d1 and d2 (without
        // jumping back it is some 3,000 leaves).
        assert!(steps <= 60, "{clash}: {steps} steps");
    }
}

/// Under `--upgrade` every present package may move, but an unnamed one keeps every thorn it has
/// enabled: a clash with a base repo's thorn is still found at once and jumped back from, not
/// searched through chronologically (some 3,900 nodes in this tree).
#[test]
fn under_upgrade_a_clash_with_a_base_thorn_still_refuses_quickly() {
    let upgrade = Options { upgrade: true, ..Options::default() };
    let (result, steps, time) = timed_with(None, upgrade);
    eprintln!("realistic installation, --upgrade: {steps} steps, {time:?}");
    result.unwrap();
    assert!(steps <= 100, "{steps} steps");
    for clash in ["new/top", "new/d3"] {
        let (result, steps, time) = timed_with(Some(clash), upgrade);
        eprintln!("clash in {clash}, --upgrade: {steps} steps, {time:?}");
        let e = result.expect_err("a clash");
        assert_eq!(e.downcast_ref::<Refusal>().map(variant).as_deref(), Some("SameThorn"), "{e}");
        assert!(steps <= 60, "{clash}: {steps} steps");
    }
}

/// Where a fork could still take a conflicting package's place, the search may not jump back
/// past anything: it goes on chronologically (slower, never wrong), and tells the caller it is
/// still going.
#[test]
fn a_search_that_cannot_jump_back_still_finishes_and_ticks() {
    let mut facts = FixtureFacts::default();
    let base = |i: usize| {
        let thorns = vec![(format!("T{i}x0"), sized_facts(&format!("T{i}x0"), &[]))];
        let r = synthetic_release(&format!("base/pkg{i}"), "1.0.0", &thorns, vec![]);
        let enabled = [format!("T{i}x0")].into_iter().collect();
        Present { release: r, enabled, origin: Origin::Base { repo: format!("pkg{i}") }, libraries: vec![] }
    };
    let present = vec![base(0), base(1)];
    let one = |name: &str| vec![(name.to_owned(), sized_facts(name, &[]))];
    // d3's every release asks the base thorn T0x0: a clash. d2's oldest release brings a fork of
    // d3 that never stands in, which is enough to make d3 displaceable.
    for d in 1..=3 {
        let id = format!("new/d{d}");
        let thorns = if d == 3 { one("T0x0") } else { one(&format!("D{d}")) };
        let releases = (0..3)
            .rev()
            .map(|v| {
                let claims = if d == 2 && v == 0 { vec![dependency("new/f", "*", &["F"])] } else { vec![] };
                synthetic_release(&id, &format!("1.{v}.0"), &thorns, claims)
            })
            .collect();
        facts.releases.insert(PkgId::parse(&id).unwrap(), releases);
    }
    let mut fork = synthetic_release("new/f", "1.0.0", &one("F"), vec![]);
    fork.claims.push(ClaimInfo {
        kind: "fork-of".into(),
        target: "new/d3".into(),
        range: Some("=9.9.9".into()),
        sha: None,
        needs: vec![],
        into: None,
        needed_by: vec![],
        alternatives: vec![],
        note: None,
    });
    facts.releases.insert(PkgId::parse("new/f").unwrap(), vec![fork]);
    let claims = vec![dependency("new/d1", "^1", &["D1"]), dependency("new/d2", "^1", &["D2"]), dependency("new/d3", "^1", &["T0x0"])];
    let releases = (0..3).rev().map(|v| synthetic_release("new/top", &format!("1.{v}.0"), &one("Top"), claims.clone())).collect();
    facts.releases.insert(PkgId::parse("new/top").unwrap(), releases);
    let specs = vec![NamedSpec::parse("new/top").unwrap()];
    let rules = Rules::default();
    let request = Request { specs: &specs, present: &present, rules: &rules, options: Options::default() };
    let e = resolve(&request, &mut facts).unwrap_err();
    let steps = super::resolve::STEPS.with(|s| s.get());
    assert_eq!(e.downcast_ref::<Refusal>().map(variant).as_deref(), Some("SameThorn"), "{e}");
    assert!(steps > 64, "{steps} steps: no chronological search?");
    assert_eq!(facts.ticks.len(), steps / 64, "a tick every 64 steps: {:?}", facts.ticks);
    assert!(facts.ticks.iter().enumerate().all(|(i, t)| *t == 64 * (i + 1)));
}

/// A library placement of a package with thorns names the claim to ignore
/// when there is one, and no empty `--ignore-claim` when there is none.
#[test]
fn not_a_library_names_a_usable_way_past() {
    let refusal = |ignore: Option<&str>| Refusal::NotALibrary {
        target: PkgId::parse("et/carpetx").unwrap(),
        by: "max/kadath@1.0.0".into(),
        into: "src/carpetx".into(),
        ignore: ignore.map(str::to_owned),
    };
    let with = refusal(Some("max/kadath:dependency:et/carpetx")).to_string();
    assert!(with.ends_with("Pass --ignore-claim max/kadath:dependency:et/carpetx (and tell the package's authors)"), "{with}");
    let without = refusal(None).to_string();
    assert!(!without.contains("--ignore-claim") && without.ends_with("Tell the package's authors"), "{without}");
}
