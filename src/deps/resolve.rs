//! The resolver: named specs plus the installation's present packages → a [`Plan`], by a
//! deterministic backtracking search.
//!
//! The search state is an assignment: a release for each package decided so far, and an
//! alternative for each any-of decided so far. Everything else (which packages are in play, the
//! needs and their takers, the enabled thorns, the constraints) is derived from the assignment
//! alone, every time, so nothing stale survives a change of mind:
//! - needs come from the named specs and from the claims of the releases in play; a present
//!   package that is not changing turns its claims into passive constraints (binding a package
//!   that moves; a present package that never comes up is checked against them only once the
//!   assignment is complete);
//! - a need is taken by its target, a package an override says satisfies it, or a fork in play
//!   that stands in for it; a target nobody takes is required, and has to be decided.
//!
//! Decisions, in a fixed order: packages first (named ones, then the rest, each group by id),
//! then any-of dependencies (by dependent, then declaration order). A package's candidates come
//! in order of preference: a present package that may stay tries its present release first;
//! otherwise newest first, approved before awaiting review; yanked only for an exact constraint.
//! An any-of tries its alternatives in the order of its rules: one present at a release that
//! fits, one already in play, then the listed order ("conflicts with nothing" being "the rest of
//! the search succeeds").
//!
//! Fork stand-ins, conflicts, `--replace`, libraries and the CCL check are checks on the state;
//! a failure backtracks. When the search fails, the refusal is the one found deepest (a specific
//! one over a bare "these do not fit together"); past a step budget scaled with the number of
//! packages, it is "no consistent set" with the constraints of that failure.
// Plan §2.2.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use cactup_ecosystem_common::api::{AlternativeInfo, ClaimInfo};
use cactup_ecosystem_common::version::{TrackedReq, TrackedVersion};
use indexmap::{IndexMap, IndexSet};

use super::plan::{Choice, ChoiceRule, Library, Placement, Plan, Record, Replaced, Step, Untrusted};
use super::refusal::{Live, Refusal, Unmet};
use super::{
    claim_target, conflict, fork_claims, forks, short, target_text, At, Facts, Fit, NamedSpec, Origin, PkgId,
    Present, Release, Req, Request, Requirement, Requirements, Rules, Why,
};
use crate::Res;
use anyhow::bail;

/// Search steps before "no consistent set", at the least; more with more packages.
pub(super) const MIN_STEPS: usize = 20_000;

/// Resolves `request` against the registry `facts` gives, checking CCL requirements with `ccl`.
/// A refusal comes back as a [`Refusal`] inside the error (downcast to read it); anything else
/// is a failure of `facts` or an interrupt.
pub fn resolve(request: &Request, facts: &mut dyn Facts, ccl: &dyn Requirements) -> Res<Plan> {
    let mut present: BTreeMap<PkgId, Present> = BTreeMap::new();
    for p in request.present {
        let mut p = p.clone();
        request.rules.apply(&mut p.release);
        present.insert(p.release.id.clone(), p);
    }
    let mut s = Search {
        request,
        cat: Catalog::new(facts, request.rules),
        ccl,
        specs: Vec::new(),
        holders: BTreeMap::new(),
        steps: 0,
        budget: MIN_STEPS.max(2_000 * (present.len() + request.specs.len() + 1)),
        exhausted: false,
        best: None,
        present,
    };
    s.cat.ensure(request.specs.iter().map(|x| x.id.clone()))?;
    // A named tip is read once, up front: the need is for the commit it is at now.
    for spec in request.specs {
        let req = match &spec.req {
            Req::Tip(tip) if s.cat.known(&spec.id) => Req::Commit(s.cat.snapshot(&spec.id, tip)?.commit),
            req => req.clone(),
        };
        s.specs.push(NamedSpec { id: spec.id.clone(), req, thorns: spec.thorns.clone() });
    }
    s.holders = s.present_holders();
    let mut assign = Assign::default();
    if let Some(plan) = s.search(&mut assign)? {
        return Ok(plan);
    }
    Err(s.refusal()?.into())
}

/// The releases among `candidates` that meet every requirement: usable (or `present`, which is
/// installed whatever its state), fitting, and yanked only for an exact requirement.
fn eligible<'r>(candidates: &'r [Release], reqs: &[Req], present: Option<&Release>) -> Vec<&'r Release> {
    candidates
        .iter()
        .filter(|r| r.usable || present.is_some_and(|p| p.same(r)))
        .filter(|r| reqs.iter().all(|q| q.fit(r) != Fit::No))
        .filter(|r| !r.yanked || reqs.iter().any(|q| q.is_exact() && q.fit(r) == Fit::Yes))
        .collect()
}

/// In order of preference: approved before awaiting review, then newest first (versions by
/// semver, above any release at no version); ties keep the order they came in.
fn preferred(mut rs: Vec<&Release>) -> Vec<&Release> {
    let rank = |r: &Release| (r.pending, std::cmp::Reverse(r.at.version().map(|v| v.version.clone())));
    rs.sort_by_key(|r| rank(r));
    rs
}

/// The registry as the resolver sees it: releases with the rules applied, loaded once.
struct Catalog<'a> {
    facts: &'a mut dyn Facts,
    rules: &'a Rules,
    /// `None`: the Cactarium has no such package.
    loaded: BTreeMap<PkgId, Option<Vec<Release>>>,
    /// Snapshots read, by package and what was asked (a commit or a tip), and by the commit found.
    snapshots: BTreeMap<(PkgId, String), Release>,
    untrusted: BTreeSet<(PkgId, String)>,
}

impl<'a> Catalog<'a> {
    fn new(facts: &'a mut dyn Facts, rules: &'a Rules) -> Catalog<'a> {
        let (loaded, snapshots, untrusted) = (BTreeMap::new(), BTreeMap::new(), BTreeSet::new());
        Catalog { facts, rules, loaded, snapshots, untrusted }
    }

    /// Loads every package not loaded yet, in one wave.
    fn ensure(&mut self, ids: impl IntoIterator<Item = PkgId>) -> Res<()> {
        let want: BTreeSet<PkgId> =
            ids.into_iter().filter(|id| !id.is_repo() && !self.loaded.contains_key(id)).collect();
        if want.is_empty() {
            return Ok(());
        }
        if gix::interrupt::is_triggered() {
            bail!("Interrupted");
        }
        let want: Vec<PkgId> = want.into_iter().collect();
        self.facts.load(&want)?;
        let rules = self.rules;
        for id in want {
            let releases = self.facts.releases(&id).map(|rs| {
                rs.iter()
                    .cloned()
                    .map(|mut r| {
                        rules.apply(&mut r);
                        r
                    })
                    .collect()
            });
            self.loaded.insert(id, releases);
        }
        Ok(())
    }

    fn loaded(&self, id: &PkgId) -> bool {
        id.is_repo() || self.loaded.contains_key(id)
    }

    fn releases(&self, id: &PkgId) -> &[Release] {
        self.loaded.get(id).and_then(|r| r.as_deref()).unwrap_or(&[])
    }

    fn known(&self, id: &PkgId) -> bool {
        matches!(self.loaded.get(id), Some(Some(_)))
    }

    fn is_flesh(&self, id: &PkgId) -> bool {
        self.releases(id).iter().any(|r| r.flesh)
    }

    /// Every published version with its commit, for `ForkOf::stands_in_for`.
    fn published(&self, id: &PkgId) -> Vec<(TrackedVersion, Option<String>)> {
        let published = |r: &Release| r.at.version().map(|v| (v.clone(), Some(r.commit.clone())));
        self.releases(id).iter().filter_map(published).collect()
    }

    fn snapshot(&mut self, id: &PkgId, at: &str) -> Res<Release> {
        let key = (id.clone(), at.to_owned());
        if let Some(r) = self.snapshots.get(&key) {
            return Ok(r.clone());
        }
        let snap = self.facts.snapshot(id, at)?;
        let mut release = snap.release;
        // What was asked must be what came back: a commit that id names (a version frozen there
        // answers as that version), or the tip asked for.
        let answered = match &release.at {
            At::Tip { name, .. } => name == at,
            _ => cactup_ecosystem_common::slug::is_commit_prefix(at) && release.commit.starts_with(at),
        };
        if !answered {
            let (label, commit) = (release.label(), &release.commit);
            bail!("Asked the Cactarium for {id} at {at}, and it answered with {label} at commit {commit}");
        }
        self.rules.apply(&mut release);
        if snap.untrusted {
            self.untrusted.insert((id.clone(), release.commit.clone()));
        }
        // A tip read is also the read at the commit it found.
        self.snapshots.insert((id.clone(), release.commit.clone()), release.clone());
        self.snapshots.insert(key, release.clone());
        Ok(release)
    }

    /// The served releases of `t`, plus the read at each commit `reqs` name that no served
    /// release is at.
    fn candidates(&mut self, t: &PkgId, reqs: &[Req]) -> Res<Vec<Release>> {
        let mut candidates: Vec<Release> = self.releases(t).to_vec();
        for q in reqs {
            if let Req::Commit(sha) = q
                && self.known(t)
                && !candidates.iter().any(|r| r.commit.starts_with(sha.as_str()))
            {
                candidates.push(self.snapshot(t, sha)?);
            }
        }
        Ok(candidates)
    }
}

/// What has been decided.
#[derive(Clone, Debug, Default)]
struct Assign {
    releases: BTreeMap<PkgId, Release>,
    /// (dependent, index of the claim) → (alternative, the rule that put it first).
    any_of: BTreeMap<(PkgId, usize), (usize, ChoiceRule)>,
}

impl Assign {
    fn depth(&self) -> usize {
        self.releases.len() + self.any_of.len()
    }
}

/// Where a need comes from.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Source {
    Named,
    UpgradeAll,
    /// A release's claim: `label` is `owner/name@version`.
    Release { label: String, id: PkgId },
}

/// How a need binds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// It brings its target in, enables thorns, and constrains.
    Active,
    /// A claim of a present package that is not changing: it constrains a package that moves (it
    /// never refuses one that may not: that predates the command). Under `--no-deps`, a note.
    Passive,
    /// The same, of a present package not decided at all: checked once the assignment is complete.
    PassiveFinal,
}

#[derive(Clone, Debug)]
enum Thorns {
    All,
    Some(Vec<String>),
}

fn thorns_of(needs: &[String]) -> Thorns {
    if needs.is_empty() { Thorns::All } else { Thorns::Some(needs.to_vec()) }
}

/// The any-of an answering need comes from.
#[derive(Clone, Debug)]
struct FromAnyOf {
    key: (PkgId, usize),
    label: String,
    alternatives: Vec<PkgId>,
    rule: ChoiceRule,
}

#[derive(Clone, Debug)]
struct Need {
    source: Source,
    target: PkgId,
    req: Req,
    thorns: Thorns,
    why: Why,
    kind: Kind,
    any_of: Option<FromAnyOf>,
    /// Its source is decided (or named): it stays while the source does.
    stable: bool,
}

/// Who takes a need.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Taker {
    Itself,
    /// A package in play standing in (a fork, or one an override names).
    Other { taker: PkgId, note: String },
    /// A fork the plan requires but has not decided yet.
    Fork(PkgId),
    /// Forks in play, none of which stands in for what the need asks.
    NotStandIn { fork: PkgId },
}

/// A package in play.
#[derive(Clone, Debug)]
pub(super) struct Entry {
    pub(super) release: Release,
    pub(super) enabled: BTreeSet<String>,
    /// What the installation has; `None` for a package the plan brings in.
    pub(super) present: Option<Present>,
    /// Its place in the plan (`usize::MAX`: no step).
    pub(super) order: usize,
    /// Its release was decided (rather than a present one nothing has come to yet).
    decided: bool,
}

impl Entry {
    /// Brought in by the plan, or present at another release now.
    pub(super) fn is_new(&self) -> bool {
        self.present.as_ref().is_none_or(|p| !p.release.same(&self.release))
    }

    /// Whether this thorn is new to the installation.
    pub(super) fn thorn_is_new(&self, thorn: &str) -> bool {
        self.is_new() || self.present.as_ref().is_some_and(|p| !contains(&p.enabled, thorn))
    }

    /// Brought in, moved, or given thorns it did not have.
    pub(super) fn changed(&self) -> bool {
        self.is_new() || self.enabled.iter().any(|t| self.thorn_is_new(t))
    }

    /// The base repo it is, unchanged.
    pub(super) fn base_repo(&self) -> Option<&str> {
        match self.present.as_ref().map(|p| &p.origin) {
            Some(Origin::Base { repo }) if !self.is_new() => Some(repo),
            _ => None,
        }
    }
}

/// An any-of waiting for a decision.
#[derive(Clone, Debug)]
struct OpenAnyOf {
    key: (PkgId, usize),
    claim: ClaimInfo,
    label: String,
}

/// Everything the assignment implies.
#[derive(Clone, Debug, Default)]
struct Derived {
    chosen: BTreeMap<PkgId, Entry>,
    needs: Vec<(Need, Taker)>,
    /// The packages the plan brings in (decided or not): every taker that is not present.
    required: BTreeSet<PkgId>,
    /// Thorns needs give each taker (`""`: all of them).
    given: BTreeMap<PkgId, BTreeSet<String>>,
    touched: BTreeSet<PkgId>,
    open: Vec<PkgId>,
    open_any_of: Vec<OpenAnyOf>,
    replaced: Vec<(Replaced, PkgId, Vec<String>, bool)>,
    /// Packages whose versions are needed and not loaded yet.
    want: BTreeSet<PkgId>,
    failure: Option<Failure>,
}

/// A refusal waiting to be chosen as the one to report.
#[derive(Clone, Debug)]
enum Pending {
    Refusal(Refusal),
    /// Providers are asked for only if this is the one reported.
    Unclaimed(Vec<(String, PkgId, Requirement)>),
}

#[derive(Clone, Debug)]
struct Failure {
    refusal: Pending,
    /// The constraints behind it.
    lives: Vec<Live>,
    /// "These do not fit together", where another release would have met every constraint.
    weak: bool,
}

impl Failure {
    fn of(refusal: Refusal, lives: Vec<Live>) -> Failure {
        Failure { refusal: Pending::Refusal(refusal), lives, weak: false }
    }
}

struct Search<'a> {
    request: &'a Request<'a>,
    present: BTreeMap<PkgId, Present>,
    cat: Catalog<'a>,
    ccl: &'a dyn Requirements,
    /// The named specs, tips read.
    specs: Vec<NamedSpec>,
    /// For each present package's any-of (dependent, claim index), the alternative it holds.
    holders: BTreeMap<(PkgId, usize), usize>,
    steps: usize,
    budget: usize,
    exhausted: bool,
    /// The failure to report: (depth, specific, failure); the first of the deepest wins.
    best: Option<(usize, bool, Failure)>,
}

/// The dependency as people read it: `et/carpet@^1.2`, or `any of et/carpet, et/cactuspugh`.
pub(super) fn claim_label(c: &ClaimInfo) -> String {
    if c.alternatives.is_empty() {
        target_text(&c.target, c.range.as_deref(), c.sha.as_deref())
    } else {
        let text = |a: &AlternativeInfo| target_text(&a.target, a.range.as_deref(), a.sha.as_deref());
        let alts: Vec<String> = c.alternatives.iter().map(text).collect();
        format!("any of {}", alts.join(", "))
    }
}

fn contains(set: &BTreeSet<String>, name: &str) -> bool {
    set.iter().any(|t| t.eq_ignore_ascii_case(name))
}

fn intersects(names: &[String], set: &BTreeSet<String>) -> bool {
    names.iter().any(|n| contains(set, n))
}

/// The thorns a need names that `r` lacks.
fn missing_thorns(r: &Release, n: &Need) -> Vec<String> {
    n.names().into_iter().filter(|th| r.thorn(th).is_none()).collect()
}

/// Whether `r` fails a need: outside its range, or without a thorn it names.
fn violates(r: &Release, n: &Need) -> bool {
    n.req.fit(r) == Fit::No || !missing_thorns(r, n).is_empty()
}

/// The ids of the entries `keep` accepts.
fn ids_where(chosen: &BTreeMap<PkgId, Entry>, keep: impl Fn(&PkgId, &Entry) -> bool) -> Vec<PkgId> {
    chosen.iter().filter(|(id, e)| keep(id, e)).map(|(id, _)| id.clone()).collect()
}

impl Need {
    fn live(&self) -> Live {
        let (source, ignore) = match &self.source {
            Source::Named => ("the command line".to_owned(), None),
            Source::UpgradeAll => ("--upgrade-all".to_owned(), None),
            Source::Release { label, id } => {
                let source = if self.kind == Kind::Active { label.clone() } else { format!("installed {label}") };
                (source, Some(format!("{id}:dependency:{}", self.target)))
            }
        };
        Live { target: self.target.clone(), req: self.req.to_string(), source, ignore }
    }

    fn names(&self) -> Vec<String> {
        match &self.thorns {
            Thorns::All => Vec::new(),
            Thorns::Some(v) => v.clone(),
        }
    }

    fn label(&self) -> Option<&str> {
        match &self.source {
            Source::Release { label, .. } => Some(label),
            _ => None,
        }
    }
}

impl Search<'_> {
    fn named(&self, id: &PkgId) -> Option<&NamedSpec> {
        self.specs.iter().find(|s| &s.id == id)
    }

    /// For each any-of of each present package, the first alternative a present package holds at
    /// a release that fits (or a present fork stands in for): its passive constraint goes there.
    fn present_holders(&self) -> BTreeMap<(PkgId, usize), usize> {
        let mut out = BTreeMap::new();
        for (id, p) in &self.present {
            for (i, c) in p.release.claims.iter().enumerate() {
                if c.kind != "dependency" || c.alternatives.is_empty() {
                    continue;
                }
                let held = c.alternatives.iter().position(|a| {
                    let target = claim_target(&a.target, a.range.as_deref(), a.sha.as_deref());
                    let Ok((target, req)) = target else { return false };
                    match self.present.get(&target) {
                        Some(t) => req.fit(&t.release) != Fit::No,
                        None => self.present.values().any(|f| {
                            let published = self.cat.published(&target);
                            let stands = |fork: &cactup_ecosystem_common::manifest::ForkOf| {
                                target.slug(&req).is_some_and(|dep| fork.stands_in_for(&dep, &published))
                            };
                            fork_claims(&f.release, &target).iter().any(stands)
                        }),
                    }
                });
                if let Some(alt) = held {
                    out.insert((id.clone(), i), alt);
                }
            }
        }
        out
    }

    fn record(&mut self, depth: usize, f: Failure) {
        let specific = !f.weak;
        if self.best.as_ref().is_none_or(|(d, s, _)| (depth, specific) > (*d, *s)) {
            self.best = Some((depth, specific, f));
        }
    }

    fn refusal(&mut self) -> Res<Refusal> {
        let Some((_, _, f)) = self.best.take() else {
            return Ok(Refusal::NoConsistentSet { constraints: Vec::new() });
        };
        if self.exhausted {
            return Ok(Refusal::NoConsistentSet { constraints: f.lives });
        }
        Ok(match f.refusal {
            Pending::Refusal(r) => r,
            Pending::Unclaimed(unmet) => {
                let mut out: Vec<Unmet> = Vec::new();
                let mut asked: BTreeMap<Requirement, Vec<super::Provider>> = BTreeMap::new();
                for (thorn, package, requirement) in unmet {
                    if gix::interrupt::is_triggered() {
                        bail!("Interrupted");
                    }
                    if !asked.contains_key(&requirement) {
                        let found = self.cat.facts.providers(&requirement)?;
                        asked.insert(requirement.clone(), found);
                    }
                    let providers = asked[&requirement].clone();
                    out.push(Unmet { thorn, package, requirement, providers });
                }
                Refusal::Unclaimed(out)
            }
        })
    }

    /// One node of the search: `Some(plan)` once a complete assignment passes every check.
    fn search(&mut self, a: &mut Assign) -> Res<Option<Plan>> {
        if self.exhausted {
            return Ok(None);
        }
        if gix::interrupt::is_triggered() {
            bail!("Interrupted");
        }
        self.steps += 1;
        if self.steps > self.budget {
            self.exhausted = true;
            return Ok(None);
        }
        let depth = a.depth();
        let d = self.derive_loaded(a)?;
        if let Some(f) = self.check(&d, false) {
            self.record(depth, f);
            return Ok(None);
        }
        if let Some(t) = d.open.first().cloned() {
            let candidates = match self.candidates(&t, &d)? {
                Ok(c) => c,
                Err(f) => {
                    self.record(depth, f);
                    return Ok(None);
                }
            };
            for c in candidates {
                a.releases.insert(t.clone(), c);
                let found = self.search(a)?;
                a.releases.remove(&t);
                if found.is_some() || self.exhausted {
                    return Ok(found);
                }
            }
            return Ok(None);
        }
        if let Some(open) = d.open_any_of.first().cloned() {
            let order = match self.alternatives(&open, &d)? {
                Ok(o) => o,
                Err(f) => {
                    self.record(depth, f);
                    return Ok(None);
                }
            };
            for (i, rule) in order {
                a.any_of.insert(open.key.clone(), (i, rule));
                let found = self.search(a)?;
                a.any_of.remove(&open.key);
                if found.is_some() || self.exhausted {
                    return Ok(found);
                }
            }
            return Ok(None);
        }
        match self.complete(d)? {
            Ok(plan) => Ok(Some(plan)),
            Err(f) => {
                self.record(depth, f);
                Ok(None)
            }
        }
    }

    /// [`Self::derive`], loading what it finds it needs, in waves, until it needs nothing more.
    fn derive_loaded(&mut self, a: &Assign) -> Res<Derived> {
        loop {
            let d = self.derive(a);
            if d.want.is_empty() {
                return Ok(d);
            }
            self.cat.ensure(d.want.iter().cloned())?;
        }
    }

    /// The state the assignment implies: a fixpoint over which packages are required and which
    /// thorns are enabled, since each feeds the other.
    fn derive(&self, a: &Assign) -> Derived {
        let mut required = BTreeSet::new();
        let mut given = BTreeMap::new();
        let mut d = Derived::default();
        for _ in 0..64 {
            d = self.derive_once(a, &required, &given);
            if d.required == required && d.given == given {
                break;
            }
            required = d.required.clone();
            given = d.given.clone();
        }
        let named_first = |id: &PkgId| (self.named(id).is_none(), id.clone());
        d.open.sort_by_key(|id| named_first(id));
        d
    }

    fn derive_once(
        &self,
        a: &Assign,
        required: &BTreeSet<PkgId>,
        given: &BTreeMap<PkgId, BTreeSet<String>>,
    ) -> Derived {
        let options = self.request.options;
        let mut d = Derived::default();
        // The packages in play, and the thorns they have.
        for (id, p) in &self.present {
            let (release, decided) = match a.releases.get(id) {
                Some(r) => (r.clone(), true),
                None => (p.release.clone(), false),
            };
            let whole = self.named(id).is_some_and(|s| s.thorns.is_none());
            let enabled = if p.release.same(&release) {
                p.enabled.clone()
            } else if whole {
                release.thorn_names()
            } else {
                p.enabled.iter().filter_map(|n| release.thorn(n).map(|t| t.thorn.clone())).collect()
            };
            let entry = Entry { release, enabled, present: Some(p.clone()), order: usize::MAX, decided };
            d.chosen.insert(id.clone(), entry);
        }
        for id in required {
            if let (None, Some(r)) = (self.present.get(id), a.releases.get(id)) {
                let (release, enabled) = (r.clone(), BTreeSet::new());
                let entry = Entry { release, enabled, present: None, order: usize::MAX, decided: true };
                d.chosen.insert(id.clone(), entry);
            }
        }
        for (id, names) in given {
            let Some(e) = d.chosen.get_mut(id) else { continue };
            let all = names.contains("");
            let add: Vec<String> = (e.release.thorns.iter())
                .filter(|t| all || contains(names, &t.thorn))
                .map(|t| t.thorn.clone())
                .collect();
            e.enabled.extend(add);
        }
        self.replace_fleshes(&mut d);
        if options.replace {
            self.absorb(&mut d);
        }
        // The needs, in a fixed order: named, --upgrade-all, then each package's claims.
        let mut needs: Vec<Need> = Vec::new();
        for spec in &self.specs {
            let thorns = spec.thorns.clone().map_or(Thorns::All, Thorns::Some);
            let (target, req) = (spec.id.clone(), spec.req.clone());
            needs.push(Need {
                source: Source::Named,
                target,
                req,
                thorns,
                why: Why::Named,
                kind: Kind::Active,
                any_of: None,
                stable: true,
            });
        }
        if options.upgrade_all {
            for (id, p) in &self.present {
                let (At::Version(v), true) = (&p.release.at, d.chosen.contains_key(id)) else { continue };
                // Any version on the track it is on.
                let mut req = TrackedReq::any();
                req.alts[0].track = v.track.clone();
                let thorns = Thorns::Some(p.enabled.iter().cloned().collect());
                needs.push(Need {
                    source: Source::UpgradeAll,
                    target: id.clone(),
                    req: Req::Range(req),
                    thorns,
                    why: Why::UpgradeAll,
                    kind: Kind::Active,
                    any_of: None,
                    stable: true,
                });
            }
        }
        let (mut open_any_of, mut failure) = (Vec::new(), d.failure.take());
        for (id, e) in &d.chosen {
            self.claim_needs(a, id, e, &mut needs, &mut open_any_of, &mut failure);
        }
        d.open_any_of = open_any_of;
        d.failure = failure;
        // Who takes each need.
        let mut new_required: BTreeSet<PkgId> = BTreeSet::new();
        let mut new_given: BTreeMap<PkgId, BTreeSet<String>> = BTreeMap::new();
        for need in needs {
            let taker = self.taker(&need, &d, required);
            let give = |given: &mut BTreeMap<PkgId, BTreeSet<String>>, who: &PkgId| {
                let names = match &need.thorns {
                    Thorns::All => BTreeSet::from([String::new()]),
                    Thorns::Some(v) => v.iter().cloned().collect(),
                };
                given.entry(who.clone()).or_default().extend(names);
            };
            if need.kind == Kind::Active {
                match &taker {
                    Taker::Itself => {
                        if !self.present.contains_key(&need.target) {
                            new_required.insert(need.target.clone());
                        }
                        if d.chosen.get(&need.target).is_some_and(|e| e.present.is_some() && !e.decided) {
                            d.touched.insert(need.target.clone());
                        }
                        give(&mut new_given, &need.target);
                        if !d.chosen.contains_key(&need.target) && !self.cat.loaded(&need.target) {
                            d.want.insert(need.target.clone());
                        }
                    }
                    Taker::Other { taker, .. } => {
                        if d.chosen.get(taker).is_some_and(|e| e.present.is_some() && !e.decided) {
                            d.touched.insert(taker.clone());
                        }
                        give(&mut new_given, taker);
                    }
                    Taker::Fork(f) => {
                        new_required.insert(f.clone());
                        give(&mut new_given, f);
                    }
                    Taker::NotStandIn { .. } => {}
                }
            }
            let stands_in = matches!(taker, Taker::Other { .. } | Taker::NotStandIn { .. });
            if stands_in && !self.cat.loaded(&need.target) {
                d.want.insert(need.target.clone());
            }
            d.needs.push((need, taker));
        }
        d.required = new_required;
        d.given = new_given;
        // The claim that brings the second flesh in, when one does.
        if let Some(Failure { refusal: Pending::Refusal(Refusal::TwoFleshes { second, ignore, .. }), .. }) =
            &mut d.failure
        {
            *ignore = (d.needs.iter())
                .find(|(n, _)| {
                    n.kind == Kind::Active
                        && n.label().is_some()
                        && d.chosen.get(&n.target).is_some_and(|e| &e.release.label() == second)
                })
                .and_then(|(n, _)| n.live().ignore);
        }
        // What is still to be decided.
        for id in &d.required {
            if !a.releases.contains_key(id) && !self.present.contains_key(id) {
                d.open.push(id.clone());
            }
        }
        for (id, e) in &d.chosen {
            let open = !e.decided
                && e.present.is_some()
                && (d.touched.contains(id) || self.named(id).is_some() || options.upgrade_all);
            if open {
                d.open.push(id.clone());
            }
        }
        d
    }

    /// The needs one package's claims make: active ones (a package brought in or moved: every
    /// claim that applies to its thorns; a present one not changing: those for the thorns new to
    /// it), and passive ones from a present package's other claims.
    fn claim_needs(
        &self,
        a: &Assign,
        id: &PkgId,
        e: &Entry,
        needs: &mut Vec<Need>,
        open_any_of: &mut Vec<OpenAnyOf>,
        failure: &mut Option<Failure>,
    ) {
        let options = self.request.options;
        let label = e.release.label();
        let source = Source::Release { label: label.clone(), id: id.clone() };
        let is_new = e.is_new();
        let new_thorns: BTreeSet<String> = e.enabled.iter().filter(|t| e.thorn_is_new(t)).cloned().collect();
        for (i, c) in e.release.claims.iter().enumerate() {
            if c.kind != "dependency" || c.into.is_some() {
                continue;
            }
            let applies = c.needed_by.is_empty() || intersects(&c.needed_by, &e.enabled);
            let for_new_thorns = !c.needed_by.is_empty() && intersects(&c.needed_by, &new_thorns);
            let active = !options.no_deps && if is_new { applies } else { for_new_thorns };
            let passive = !active && applies && !is_new && e.present.is_some();
            if !active && !passive {
                continue;
            }
            let kind = match () {
                _ if active => Kind::Active,
                _ if e.decided => Kind::Passive,
                _ => Kind::PassiveFinal,
            };
            let ignore = format!("{id}:dependency:{}", c.target);
            let (alt, any_of) = if c.alternatives.is_empty() {
                (None, None)
            } else if active {
                let key = (id.clone(), i);
                let Some((alt, rule)) = a.any_of.get(&key) else {
                    open_any_of.push(OpenAnyOf { key, claim: c.clone(), label: label.clone() });
                    continue;
                };
                let alternatives =
                    c.alternatives.iter().filter_map(|x| PkgId::parse(&x.target).ok()).collect();
                let from = FromAnyOf { key, label: claim_label(c), alternatives, rule: *rule };
                (Some(&c.alternatives[*alt]), Some(from))
            } else {
                match self.holders.get(&(id.clone(), i)) {
                    Some(alt) => (Some(&c.alternatives[*alt]), None),
                    None => continue,
                }
            };
            let (target, range, sha, thorns) = match alt {
                Some(x) => (&x.target, x.range.as_deref(), x.sha.as_deref(), &x.needs),
                None => (&c.target, c.range.as_deref(), c.sha.as_deref(), &c.needs),
            };
            let (target, req) = match claim_target(target, range, sha) {
                Ok(x) => x,
                Err(detail) => {
                    // A passive claim nobody can read predates this command; an active one stops it.
                    if active && failure.is_none() {
                        let (by, claim) = (label.clone(), claim_label(c));
                        let r = Refusal::BadClaim { by, claim, detail, ignore };
                        *failure = Some(Failure::of(r, Vec::new()));
                    }
                    continue;
                }
            };
            let why = Why::Needed {
                by: label.clone(),
                by_id: id.clone(),
                claim: claim_label(c),
                req: req.to_string(),
                needed_by: c.needed_by.clone(),
                rule: any_of.as_ref().map(|f| f.rule),
            };
            let thorns = thorns_of(thorns);
            let (source, stable) = (source.clone(), e.decided);
            needs.push(Need { source, target, req, thorns, why, kind, any_of, stable });
        }
    }

    /// Who takes a need: a named spec its own target; else the target when in play; else a
    /// package an override says satisfies it; else a fork in play that stands in (any of its
    /// fork-of claims, the forks in id order); else an undecided fork the plan requires; else the
    /// target itself (also when the forks in play do not stand in for a flesh: the flesh is
    /// replaced).
    fn taker(&self, need: &Need, d: &Derived, required: &BTreeSet<PkgId>) -> Taker {
        let t = &need.target;
        if need.source == Source::Named || d.chosen.contains_key(t) {
            return Taker::Itself;
        }
        for x in self.request.rules.satisfiers(t) {
            if d.chosen.contains_key(x) {
                let note = format!("{x} stands in for {t} (a claims override)");
                return Taker::Other { taker: x.clone(), note };
            }
        }
        let published = self.cat.published(t);
        let mut not_standing: Option<PkgId> = None;
        for (id, e) in &d.chosen {
            let claims = fork_claims(&e.release, t);
            if claims.is_empty() {
                continue;
            }
            let stands = |fork: &cactup_ecosystem_common::manifest::ForkOf| {
                t.slug(&need.req).is_some_and(|dep| fork.stands_in_for(&dep, &published))
            };
            if claims.iter().any(stands) {
                let note = format!("{} stands in for {t}", e.release.label());
                return Taker::Other { taker: id.clone(), note };
            }
            not_standing.get_or_insert_with(|| id.clone());
        }
        if need.kind == Kind::Active {
            for f in required.iter().filter(|f| !d.chosen.contains_key(*f)) {
                if self.cat.releases(f).iter().any(|r| forks(r, t)) {
                    return Taker::Fork(f.clone());
                }
            }
        }
        match not_standing {
            Some(_) if self.cat.is_flesh(t) => Taker::Itself,
            Some(fork) => Taker::NotStandIn { fork },
            None => Taker::Itself,
        }
    }

    /// A flesh the plan brings in replaces the flesh the installation has (a base repo, or an
    /// added flesh package); two fleshes brought in are refused.
    fn replace_fleshes(&self, d: &mut Derived) {
        let planned = ids_where(&d.chosen, |_, e| e.present.is_none() && e.release.flesh);
        match planned.as_slice() {
            [] => {}
            [by] => {
                let fleshes = ids_where(&d.chosen, |id, e| id != by && e.release.flesh);
                for id in fleshes {
                    let e = d.chosen.remove(&id).expect("listed above");
                    let replaced = match e.present.map(|p| p.origin) {
                        Some(Origin::Base { repo }) => Replaced::Base { repo },
                        _ => Replaced::Added { id },
                    };
                    d.replaced.push((replaced, by.clone(), Vec::new(), true));
                }
            }
            [first, second, ..] => {
                let (first, second) = (d.chosen[first].release.label(), d.chosen[second].release.label());
                let r = Refusal::TwoFleshes { first, second, ignore: None };
                d.failure.get_or_insert(Failure::of(r, Vec::new()));
            }
        }
    }

    /// Under `--replace`, a package the plan brings in or moves takes the place of every unchanged
    /// base package it forks, with every thorn the installation has enabled from it.
    fn absorb(&self, d: &mut Derived) {
        let newcomers = ids_where(&d.chosen, |_, e| e.is_new());
        for n in newcomers {
            let Some(release) = d.chosen.get(&n).map(|e| e.release.clone()) else { continue };
            let forked_base = |id: &PkgId, e: &Entry| *id != n && e.base_repo().is_some() && forks(&release, id);
            let related = ids_where(&d.chosen, forked_base);
            for u in related {
                let ue = d.chosen.remove(&u).expect("listed above");
                let repo = ue.base_repo().unwrap_or_default().to_owned();
                let missing: Vec<String> =
                    ue.enabled.iter().filter(|t| release.thorn(t).is_none()).cloned().collect();
                if !missing.is_empty() {
                    let (by, by_label) = (n.clone(), release.label());
                    let (upstream, base_repo, thorns) = (u, repo, missing);
                    let r = Refusal::ReplacementLacksThorns { by, by_label, upstream, base_repo, thorns };
                    d.failure.get_or_insert(Failure::of(r, Vec::new()));
                    return;
                }
                let names: Vec<String> =
                    ue.enabled.iter().filter_map(|t| release.thorn(t).map(|x| x.thorn.clone())).collect();
                d.chosen.get_mut(&n).expect("a newcomer").enabled.extend(names);
                d.replaced.push((Replaced::Base { repo }, n.clone(), Vec::new(), false));
            }
        }
    }

    /// Whether a present package may move: named, under `--upgrade-all`, touched under
    /// `--upgrade`, or a flesh. Never a pseudo-package.
    fn movable(&self, t: &PkgId, d: &Derived) -> bool {
        let options = self.request.options;
        !t.is_repo()
            && (self.named(t).is_some()
                || options.upgrade_all
                || (options.upgrade && (d.touched.contains(t) || d.chosen.get(t).is_some_and(|e| e.decided)))
                || d.chosen.get(t).is_some_and(|e| e.release.flesh))
    }

    /// Whether a need binds in this check: active ones always; passive ones of decided packages
    /// (not under `--no-deps`); those of packages never decided only once the assignment is
    /// complete.
    fn binds(&self, need: &Need, last: bool) -> bool {
        match need.kind {
            Kind::Active => true,
            Kind::Passive => !self.request.options.no_deps,
            Kind::PassiveFinal => last && !self.request.options.no_deps,
        }
    }

    /// The first failure in the state: one found while deriving it, a fork that does not stand
    /// in, a stand-in lacking thorns, or a decided package outside its constraints.
    fn check(&self, d: &Derived, last: bool) -> Option<Failure> {
        if let Some(f) = &d.failure {
            return Some(f.clone());
        }
        for (need, taker) in &d.needs {
            if !self.binds(need, last) {
                continue;
            }
            match taker {
                Taker::NotStandIn { fork } => {
                    let Some(e) = d.chosen.get(fork) else { continue };
                    let (fork, fork_label, live) = (fork.clone(), e.release.label(), need.live());
                    let r = Refusal::ForkDoesNotStandIn { fork, fork_label, live };
                    return Some(Failure::of(r, vec![need.live()]));
                }
                Taker::Other { taker, .. } => {
                    let Some(e) = d.chosen.get(taker) else { continue };
                    let missing = missing_thorns(&e.release, need);
                    if !missing.is_empty() {
                        let (taker, live) = (e.release.label(), need.live());
                        let r = Refusal::MissingThorns { taker, thorns: missing, live };
                        return Some(Failure::of(r, vec![need.live()]));
                    }
                }
                Taker::Itself | Taker::Fork(_) => {}
            }
        }
        for (t, e) in &d.chosen {
            if !e.decided {
                continue;
            }
            let fixed = e.present.is_some() && !self.movable(t, d);
            let on_t: Vec<&Need> = (d.needs.iter())
                .filter(|(n, k)| &n.target == t && *k == Taker::Itself && self.binds(n, last))
                .filter(|(n, _)| !(fixed && n.kind != Kind::Active))
                .map(|(n, _)| n)
                .collect();
            let Some(first) = on_t.iter().find(|n| violates(&e.release, n)) else { continue };
            let lives: Vec<Live> = on_t.iter().map(|n| n.live()).collect();
            if fixed {
                let (installed, live) = (e.release.label(), first.live());
                let missing = missing_thorns(&e.release, first);
                let r = if first.req.fit(&e.release) != Fit::No && !missing.is_empty() {
                    Refusal::PresentLacksThorns { installed, thorns: missing, live }
                } else {
                    Refusal::PresentOutsideRange { installed, live }
                };
                return Some(Failure::of(r, lives));
            }
            // Would another release have met them all? Then this is only a wrong turn.
            let mut releases: Vec<Release> = self.cat.releases(t).to_vec();
            if let Some(p) = &e.present {
                releases.push(p.release.clone());
            }
            releases.push(e.release.clone());
            let reqs: Vec<Req> = on_t.iter().map(|n| n.req.clone()).collect();
            let fits_all = eligible(&releases, &reqs, e.present.as_ref().map(|p| &p.release))
                .into_iter()
                .any(|r| on_t.iter().all(|n| !violates(r, n)));
            if fits_all {
                let r = Refusal::NoConsistentSet { constraints: lives.clone() };
                return Some(Failure { refusal: Pending::Refusal(r), lives, weak: true });
            }
            return Some(Failure::of(self.no_release(t, lives.clone()), lives));
        }
        None
    }

    fn no_release(&self, t: &PkgId, constraints: Vec<Live>) -> Refusal {
        if self.cat.is_flesh(t) {
            Refusal::FleshUnmet { target: t.clone(), constraints }
        } else {
            Refusal::NoReleaseFits { target: t.clone(), constraints }
        }
    }

    /// The releases to try for `t`, in order of preference, or why there are none. Only needs
    /// from decided sources narrow them (the others may still change); every need is checked.
    fn candidates(&mut self, t: &PkgId, d: &Derived) -> Res<Result<Vec<Release>, Failure>> {
        let on_t: Vec<&Need> =
            d.needs.iter().filter(|(n, k)| &n.target == t && *k == Taker::Itself).map(|(n, _)| n).collect();
        let lives: Vec<Live> = on_t.iter().map(|n| n.live()).collect();
        let narrowing: Vec<&Need> =
            on_t.iter().copied().filter(|n| n.stable && self.binds(n, false)).collect();
        let reqs: Vec<Req> = narrowing.iter().map(|n| n.req.clone()).collect();
        let mut required: Vec<String> = narrowing.iter().flat_map(|n| n.names()).collect();
        let named = self.named(t).cloned();
        let present = self.present.get(t).cloned();
        if let Some(p) = &present {
            if !self.movable(t, d) {
                return Ok(Ok(vec![p.release.clone()]));
            }
            // An unnamed move carries every thorn the package has enabled.
            if named.is_none() {
                required.extend(p.enabled.iter().cloned());
            }
        }
        if let Some(Req::Tip(_)) = named.as_ref().map(|s| &s.req) {
            return Ok(Err(Failure::of(Refusal::NotRegistered { target: t.clone(), by: None }, lives)));
        }
        self.cat.ensure([t.clone()])?;
        if present.is_none() && !self.cat.known(t) {
            let by = on_t.iter().find(|n| n.source != Source::Named).map(|n| n.live());
            return Ok(Err(Failure::of(Refusal::NotRegistered { target: t.clone(), by }, lives)));
        }
        let mut all = self.cat.candidates(t, &reqs)?;
        if let Some(p) = &present
            && !all.iter().any(|r| r.same(&p.release))
        {
            all.push(p.release.clone());
        }
        let ok = eligible(&all, &reqs, present.as_ref().map(|p| &p.release));
        let has_required = |r: &&Release| required.iter().all(|th| r.thorn(th).is_some());
        let ok: Vec<&Release> = ok.into_iter().filter(has_required).collect();
        if !ok.is_empty() && ok.iter().all(|r| r.library) {
            let r = match on_t.iter().find_map(|n| match &n.source {
                Source::Release { label, id } => Some((label.clone(), id.clone())),
                _ => None,
            }) {
                Some((by, id)) => Refusal::BadClaim {
                    by,
                    claim: t.to_string(),
                    detail: format!("{t} is a library, and the dependency does not say where it goes (into)"),
                    ignore: format!("{id}:dependency:{t}"),
                },
                None => Refusal::NamedLibrary { target: t.clone() },
            };
            return Ok(Err(Failure::of(r, lives)));
        }
        let ok: Vec<&Release> = preferred(ok.into_iter().filter(|r| !r.library).collect());
        if ok.is_empty() {
            return Ok(Err(Failure::of(self.no_release(t, lives.clone()), lives)));
        }
        // A present package that may stay tries its present release first, unless it re-resolves
        // to its newest (a named added package, or --upgrade-all).
        let newest_first = match present.as_ref().map(|p| &p.origin) {
            None => true,
            Some(Origin::Added) => named.is_some() || self.request.options.upgrade_all,
            Some(Origin::Base { .. }) => {
                (named.is_some() && self.request.options.upgrade) || self.request.options.upgrade_all
            }
        };
        let flesh = ok.iter().any(|r| r.flesh);
        let mut out: Vec<Release> = Vec::new();
        if let Some(p) = present.as_ref().filter(|_| !newest_first || flesh)
            && let Some(r) = ok.iter().find(|r| r.same(&p.release))
        {
            out.push((*r).clone());
        }
        let rest: Vec<Release> = ok.into_iter().filter(|r| !out.iter().any(|o| o.same(r))).cloned().collect();
        out.extend(rest);
        Ok(Ok(out))
    }

    /// The alternatives of an any-of in the order its rules try them, each with its rule.
    fn alternatives(
        &mut self,
        open: &OpenAnyOf,
        d: &Derived,
    ) -> Res<Result<Vec<(usize, ChoiceRule)>, Failure>> {
        let mut alts: Vec<(PkgId, Req)> = Vec::new();
        for alt in &open.claim.alternatives {
            match claim_target(&alt.target, alt.range.as_deref(), alt.sha.as_deref()) {
                Ok(x) => alts.push(x),
                Err(detail) => {
                    let (by, claim) = (open.label.clone(), claim_label(&open.claim));
                    let ignore = format!("{}:dependency:{}", open.key.0, alt.target);
                    return Ok(Err(Failure::of(Refusal::BadClaim { by, claim, detail, ignore }, Vec::new())));
                }
            }
        }
        self.cat.ensure(alts.iter().map(|(id, _)| id.clone()))?;
        // Present: a present package at a release that fits (or a present fork standing in);
        // planned: one in play or required.
        let class = |(id, req): &(PkgId, Req)| -> Option<ChoiceRule> {
            let need = Need {
                source: Source::Release { label: open.label.clone(), id: open.key.0.clone() },
                target: id.clone(),
                req: req.clone(),
                thorns: Thorns::All,
                why: Why::Named,
                kind: Kind::Active,
                any_of: None,
                stable: true,
            };
            match self.taker(&need, d, &d.required) {
                Taker::Itself => match d.chosen.get(id) {
                    Some(e) if req.fit(&e.release) == Fit::No => None,
                    Some(e) if e.present.is_some() => Some(ChoiceRule::Present),
                    Some(_) => Some(ChoiceRule::Planned),
                    None if d.required.contains(id) => Some(ChoiceRule::Planned),
                    None => None,
                },
                Taker::Other { taker, .. } => {
                    let present = d.chosen.get(&taker).is_some_and(|e| e.present.is_some());
                    Some(if present { ChoiceRule::Present } else { ChoiceRule::Planned })
                }
                Taker::Fork(_) => Some(ChoiceRule::Planned),
                Taker::NotStandIn { .. } => None,
            }
        };
        let classes: Vec<Option<ChoiceRule>> = alts.iter().map(class).collect();
        let mut order: Vec<(usize, ChoiceRule)> = Vec::new();
        for want in [ChoiceRule::Present, ChoiceRule::Planned] {
            let these = classes.iter().enumerate().filter(|(_, c)| **c == Some(want));
            order.extend(these.map(|(i, _)| (i, want)));
        }
        for i in 0..alts.len() {
            if !order.iter().any(|(j, _)| *j == i) {
                let rule = if i == 0 { ChoiceRule::FirstListed } else { ChoiceRule::FirstWithoutConflict };
                order.push((i, rule));
            }
        }
        Ok(Ok(order))
    }

    /// The checks on a complete assignment, then the plan.
    fn complete(&mut self, d: Derived) -> Res<Result<Plan, Failure>> {
        if let Some(f) = self.check(&d, true) {
            return Ok(Err(f));
        }
        let mut chosen = d.chosen.clone();
        let mut replaced = d.replaced.clone();
        let (order, why, brought_by) = self.order(&d);
        for (i, id) in order.iter().enumerate() {
            if let Some(e) = chosen.get_mut(id) {
                e.order = i;
            }
        }
        if self.request.options.replace {
            replace_colliding(&mut chosen, &mut replaced);
        }
        if let Some(mut r) = conflict::conflicts(&chosen) {
            // The claim that brings the newcomer in, for the generic same-thorn refusal.
            if let Refusal::SameThorn { newcomer, base_repo: None, ignore, .. } = &mut r {
                *ignore = (d.needs.iter())
                    .find(|(n, _)| n.kind == Kind::Active && &n.target == newcomer && n.label().is_some())
                    .and_then(|(n, _)| n.live().ignore);
            }
            return Ok(Err(Failure::of(r, Vec::new())));
        }
        let libraries = match self.place_libraries(&chosen)? {
            Ok(l) => l,
            Err(f) => return Ok(Err(f)),
        };
        let unmet = conflict::unmet_requirements(&chosen, self.ccl);
        if !unmet.is_empty() {
            return Ok(Err(Failure { refusal: Pending::Unclaimed(unmet), lives: Vec::new(), weak: false }));
        }
        Ok(Ok(self.plan(&d, chosen, replaced, libraries, &order, &why, &brought_by)))
    }

    /// The order of the plan's steps: the named packages, then what their single-target
    /// dependencies bring, breadth first, then what each any-of brings (by dependent and
    /// declaration order); with each package's reason (the first need reaching it) and the
    /// dependents that brought it in.
    #[allow(clippy::type_complexity)]
    fn order(
        &self,
        d: &Derived,
    ) -> (IndexSet<PkgId>, BTreeMap<PkgId, Why>, BTreeMap<PkgId, IndexSet<String>>) {
        let active: Vec<(&Need, PkgId)> = (d.needs.iter())
            .filter(|(n, _)| n.kind == Kind::Active)
            .filter_map(|(n, t)| match t {
                Taker::Itself => Some((n, n.target.clone())),
                Taker::Other { taker, .. } => Some((n, taker.clone())),
                Taker::Fork(f) => Some((n, f.clone())),
                Taker::NotStandIn { .. } => None,
            })
            .collect();
        let by_source = |id: &PkgId| -> Vec<(&Need, PkgId)> {
            let from = |n: &Need| matches!(&n.source, Source::Release { id: s, .. } if s == id);
            active.iter().filter(|(n, _)| from(n)).cloned().collect()
        };
        let mut order: IndexSet<PkgId> = IndexSet::new();
        let mut why: BTreeMap<PkgId, Why> = BTreeMap::new();
        let mut queue: VecDeque<PkgId> = VecDeque::new();
        let mut reach = |n: &Need, t: &PkgId, order: &mut IndexSet<PkgId>, queue: &mut VecDeque<PkgId>| {
            if order.insert(t.clone()) {
                why.insert(t.clone(), n.why.clone());
                queue.push_back(t.clone());
            }
        };
        for (n, t) in active.iter().filter(|(n, _)| !matches!(n.source, Source::Release { .. })) {
            reach(n, t, &mut order, &mut queue);
        }
        let mut any_ofs: Vec<(&Need, PkgId)> =
            active.iter().filter(|(n, _)| n.any_of.is_some()).cloned().collect();
        any_ofs.sort_by_key(|(n, _)| n.any_of.as_ref().map(|f| f.key.clone()));
        let mut next_any_of = any_ofs.into_iter();
        loop {
            while let Some(p) = queue.pop_front() {
                for (n, t) in by_source(&p).iter().filter(|(n, _)| n.any_of.is_none()) {
                    reach(n, t, &mut order, &mut queue);
                }
            }
            match next_any_of.next() {
                Some((n, t)) => reach(n, &t, &mut order, &mut queue),
                None => break,
            }
        }
        // The dependents in id order, not in the order the specs were named.
        let mut brought_by: BTreeMap<PkgId, IndexSet<String>> = BTreeMap::new();
        let mut sources: Vec<&PkgId> = order.iter().collect();
        sources.sort();
        for p in sources {
            for (n, t) in by_source(p) {
                brought_by.entry(t).or_default().insert(n.label().unwrap_or_default().to_owned());
            }
        }
        (order, why, brought_by)
    }

    #[allow(clippy::too_many_arguments)]
    fn plan(
        &self,
        d: &Derived,
        chosen: BTreeMap<PkgId, Entry>,
        replaced: Vec<(Replaced, PkgId, Vec<String>, bool)>,
        libraries: Vec<Library>,
        order: &IndexSet<PkgId>,
        why: &BTreeMap<PkgId, Why>,
        brought_by: &BTreeMap<PkgId, IndexSet<String>>,
    ) -> Plan {
        let options = self.request.options;
        let mut notes: IndexSet<String> = IndexSet::new();
        // A release at no version is taken to meet a range, with a note.
        for id in order {
            let Some(e) = chosen.get(id).filter(|e| e.release.at.version().is_none()) else { continue };
            let on_id =
                |(n, k): &&(Need, Taker)| &n.target == id && *k == Taker::Itself && n.kind == Kind::Active;
            for (n, _) in d.needs.iter().filter(on_id) {
                if matches!(&n.req, Req::Range(q) if *q != TrackedReq::any()) {
                    let l = n.live();
                    notes.insert(format!("{} is at a commit no version names; taken to meet {} (from {})", e.release.label(), l.req, l.source));
                }
            }
        }
        // Under --no-deps, each present package's declared range or thorns the plan breaks.
        if options.no_deps {
            for (n, k) in &d.needs {
                let Some(e) = chosen.get(&n.target).filter(|e| e.is_new() && *k == Taker::Itself) else {
                    continue;
                };
                if n.kind != Kind::Active && violates(&e.release, n) {
                    notes.insert(format!("{} needs {} {}; --no-deps leaves it unmet", n.label().unwrap_or_default(), n.target, n.req));
                }
            }
        }
        // Who stands in for whom: a keep line for a present package, a note otherwise.
        let mut keep: IndexMap<PkgId, IndexSet<String>> = IndexMap::new();
        for (n, k) in &d.needs {
            let Taker::Other { taker, note } = k else { continue };
            if n.kind != Kind::Active {
                continue;
            }
            match chosen.get(taker) {
                Some(e) if e.present.is_some() && !e.is_new() => {
                    keep.entry(taker.clone()).or_default().insert(note.clone());
                }
                _ => {
                    notes.insert(note.clone());
                }
            }
        }
        let mut plan = Plan::default();
        let mut choices: Vec<Choice> = (d.needs.iter())
            .filter_map(|(n, k)| {
                let f = n.any_of.as_ref()?;
                let chosen = match k {
                    Taker::Other { taker, .. } => taker.clone(),
                    _ => n.target.clone(),
                };
                Some(Choice {
                    dependent: f.key.0.clone(),
                    dependent_label: n.label().unwrap_or_default().to_owned(),
                    claim: f.key.1,
                    label: f.label.clone(),
                    alternatives: f.alternatives.clone(),
                    chosen,
                    rule: f.rule,
                })
            })
            .collect();
        choices.sort_by(|a, b| (&a.dependent, a.claim).cmp(&(&b.dependent, b.claim)));
        for id in order {
            let Some(e) = chosen.get(id) else { continue };
            let named = self.named(id).is_some();
            let why = why.get(id).cloned().unwrap_or(Why::Named);
            let enabled: Vec<String> = e.enabled.iter().cloned().collect();
            let mut recorded = true;
            match &e.present {
                None => plan.steps.push(Step::Add { release: e.release.clone(), thorns: enabled, why }),
                Some(p) if e.is_new() => {
                    // A named package, a flesh, or one --upgrade-all moved says nothing more.
                    let unflagged = named || e.release.flesh || why == Why::UpgradeAll;
                    let flag = match () {
                        _ if unflagged => None,
                        _ if options.upgrade_all => Some("--upgrade-all"),
                        _ if options.upgrade => Some("--upgrade"),
                        _ => None,
                    };
                    let (from, to) = (p.release.clone(), e.release.clone());
                    plan.steps.push(Step::Upgrade { from, to, thorns: enabled, why, flag });
                    if let Origin::Base { repo } = &p.origin {
                        let replaced = Replaced::Base { repo: repo.clone() };
                        let (by, flesh, thorns) = (id.clone(), e.release.flesh, Vec::new());
                        plan.steps.push(Step::Replace { replaced, by, thorns, flesh, explicit: named });
                    }
                }
                Some(p) => {
                    let newly: Vec<String> =
                        e.enabled.iter().filter(|t| !contains(&p.enabled, t)).cloned().collect();
                    match &p.origin {
                        Origin::Added if named => {
                            plan.steps.push(Step::Readd { release: e.release.clone(), thorns: enabled })
                        }
                        _ if !newly.is_empty() => {
                            let (id, label) = (id.clone(), e.release.label());
                            plan.steps.push(Step::AddThorns { id, label, thorns: newly, why });
                        }
                        Origin::Base { .. } if named => {
                            let note = format!("{} is in the installation's base already", e.release.label());
                            plan.steps.push(Step::Keep { id: id.clone(), note });
                            recorded = false;
                        }
                        _ => recorded = false,
                    }
                }
            }
            for (what, by, thorns, flesh) in &replaced {
                if by == id {
                    let (replaced, by) = (what.clone(), by.clone());
                    let (thorns, flesh) = (thorns.clone(), *flesh);
                    plan.steps.push(Step::Replace { replaced, by, thorns, flesh, explicit: named });
                }
            }
            for note in keep.get(id).into_iter().flatten() {
                plan.steps.push(Step::Keep { id: id.clone(), note: note.clone() });
            }
            if recorded {
                let choices = choices.iter().filter(|c| &c.chosen == id).map(Choice::key).collect();
                let by = brought_by.get(id).map(|b| b.iter().cloned().collect()).unwrap_or_default();
                let requested = match () {
                    _ if named => Some(true),
                    _ if e.present.is_none() => Some(false),
                    _ => None,
                };
                let commit = e.release.commit.clone();
                plan.records.push(Record { id: id.clone(), commit, requested, by, choices });
            }
            if e.is_new() {
                if e.release.pending {
                    plan.pending.push(e.release.label());
                }
                plan.untrusted.extend(self.untrusted(&e.release));
            }
        }
        for l in &libraries {
            if l.placements.iter().any(|p| p.new) {
                if l.release.pending && !plan.pending.contains(&l.release.label()) {
                    plan.pending.push(l.release.label());
                }
                plan.untrusted.extend(self.untrusted(&l.release));
            }
        }
        plan.notes = notes.into_iter().collect();
        plan.libraries = libraries;
        plan.rebuild = plan.steps.iter().any(|s| match s {
            Step::Add { release, .. } | Step::Upgrade { to: release, .. } => release.flesh,
            Step::Replace { flesh, .. } => *flesh,
            _ => false,
        });
        plan.choices = choices;
        plan
    }

    /// The snapshot `r` is, when nobody vouches for it.
    fn untrusted(&self, r: &Release) -> Option<Untrusted> {
        if !self.cat.untrusted.contains(&(r.id.clone(), r.commit.clone())) {
            return None;
        }
        let tip = match &r.at {
            At::Tip { name, .. } => Some(name.clone()),
            _ => None,
        };
        Some(Untrusted { id: r.id.clone(), commit: r.commit.clone(), tip })
    }

    /// Every library the plan places: the libraries of packages it brings in or moves (and the
    /// libraries those go inside), at one release every placement accepts. A library an unchanged
    /// package installed stays at its commit, and a new placement must accept it (and when
    /// unchanged packages hold it at different commits, nothing can). Placement goes on under
    /// `--no-deps`.
    fn place_libraries(&mut self, chosen: &BTreeMap<PkgId, Entry>) -> Res<Result<Vec<Library>, Failure>> {
        let mut installed: BTreeMap<PkgId, Vec<(String, Placement, Live)>> = BTreeMap::new();
        for (id, e) in chosen {
            let Some(p) = e.present.as_ref().filter(|_| !e.is_new()) else { continue };
            for l in &p.libraries {
                let placement = Placement {
                    by: id.clone(),
                    by_label: e.release.label(),
                    inside: l.inside.clone(),
                    into: l.into.clone(),
                    req: None,
                    new: false,
                };
                let source = format!("installed {}", e.release.label());
                let live = Live { target: l.id.clone(), req: format!("#{}", short(&l.commit)), source, ignore: None };
                installed.entry(l.id.clone()).or_default().push((l.commit.clone(), placement, live));
            }
        }
        let mut roots: Vec<&Entry> = chosen.values().filter(|e| e.is_new()).collect();
        roots.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.release.id.cmp(&b.release.id)));
        let roots: Vec<(PkgId, Release)> =
            roots.iter().map(|e| (e.release.id.clone(), e.release.clone())).collect();
        let mut selected: BTreeMap<PkgId, Release> = BTreeMap::new();
        // Each round reads every placing release (the packages, then the libraries selected so
        // far, inside each package that placed them) and selects each library again, until
        // nothing changes.
        for _ in 0..64 {
            let mut placements: BTreeMap<PkgId, Vec<(Placement, Live)>> = BTreeMap::new();
            let mut frontier: VecDeque<(PkgId, Option<PkgId>, Release)> =
                roots.iter().map(|(id, r)| (id.clone(), None, r.clone())).collect();
            let mut seen: BTreeSet<(PkgId, PkgId)> = BTreeSet::new();
            while let Some((by, inside, r)) = frontier.pop_front() {
                for c in r.claims.iter().filter(|c| c.kind == "dependency") {
                    let Some(into) = &c.into else { continue };
                    let (label, ignore) = (r.label(), format!("{}:dependency:{}", r.id, c.target));
                    let (lib, req) = match claim_target(&c.target, c.range.as_deref(), c.sha.as_deref()) {
                        Ok(x) => x,
                        Err(detail) => {
                            let claim = claim_label(c);
                            let r = Refusal::BadClaim { by: label, claim, detail, ignore };
                            return Ok(Err(Failure::of(r, Vec::new())));
                        }
                    };
                    let (target, source) = (lib.clone(), label.clone());
                    let live = Live { target, req: req.to_string(), source, ignore: Some(ignore) };
                    let placement = Placement {
                        by: by.clone(),
                        by_label: label,
                        inside: inside.clone(),
                        into: into.clone(),
                        req: Some(req),
                        new: true,
                    };
                    placements.entry(lib.clone()).or_default().push((placement, live));
                    // An installed library's own libraries are installed with it.
                    if let Some(sel) = selected.get(&lib)
                        && !installed.contains_key(&lib)
                        && seen.insert((by.clone(), lib.clone()))
                    {
                        frontier.push_back((by.clone(), Some(lib.clone()), sel.clone()));
                    }
                }
            }
            self.cat.ensure(placements.keys().cloned())?;
            let mut next: BTreeMap<PkgId, Release> = BTreeMap::new();
            for (lib, ps) in &placements {
                let reqs: Vec<Req> = ps.iter().filter_map(|(p, _)| p.req.clone()).collect();
                let mut lives: Vec<Live> = ps.iter().map(|(_, l)| l.clone()).collect();
                let target = lib.clone();
                if let Some(held) = installed.get(lib) {
                    lives.extend(held.iter().map(|(_, _, l)| l.clone()));
                    let commits: BTreeSet<&String> = held.iter().map(|(c, _, _)| c).collect();
                    let commit = held[0].0.clone();
                    let release = self.cat.releases(lib).iter().find(|r| r.commit == commit).cloned();
                    let release = release.unwrap_or_else(|| Release {
                        id: lib.clone(),
                        at: At::Unknown(commit.clone()),
                        commit: commit.clone(),
                        tag: None,
                        url: String::new(),
                        contact: None,
                        flesh: false,
                        library: true,
                        yanked: false,
                        pending: false,
                        usable: true,
                        thorns: Vec::new(),
                        claims: Vec::new(),
                    });
                    if commits.len() > 1 || reqs.iter().any(|q| q.fit(&release) == Fit::No) {
                        let r = Refusal::LibraryClash { library: target, placements: lives.clone() };
                        return Ok(Err(Failure::of(r, lives)));
                    }
                    next.insert(lib.clone(), release);
                    continue;
                }
                let candidates = self.cat.candidates(lib, &reqs)?;
                let ok: Vec<&Release> =
                    eligible(&candidates, &reqs, None).into_iter().filter(|r| r.library).collect();
                let Some(best) = preferred(ok).first().map(|r| (*r).clone()) else {
                    let r = if !self.cat.known(lib) {
                        Refusal::NotRegistered { target, by: lives.first().cloned() }
                    } else if ps.iter().map(|(p, _)| &p.by).collect::<BTreeSet<_>>().len() > 1 {
                        Refusal::LibraryClash { library: target, placements: lives.clone() }
                    } else {
                        Refusal::NoReleaseFits { target, constraints: lives.clone() }
                    };
                    return Ok(Err(Failure::of(r, lives)));
                };
                next.insert(lib.clone(), best);
            }
            let same = next.len() == selected.len()
                && next.iter().zip(&selected).all(|((a, ra), (b, rb))| a == b && ra.same(rb));
            if same {
                let mut out = Vec::new();
                for (lib, release) in selected {
                    let new = placements.remove(&lib).unwrap_or_default();
                    let mut ps: Vec<Placement> = new.into_iter().map(|(p, _)| p).collect();
                    if let Some(held) = installed.remove(&lib) {
                        ps.extend(held.into_iter().map(|(_, p, _)| p));
                    }
                    // New placements first, then by package: not in the order specs were named.
                    let key = |p: &Placement| (!p.new, p.by.clone(), p.inside.clone(), p.into.clone());
                    ps.sort_by_key(key);
                    out.push(Library { release, placements: ps });
                }
                return Ok(Ok(out));
            }
            selected = next;
        }
        bail!("Libraries placed inside each other without end")
    }
}

/// Under `--replace`, a base repo sharing a thorn name with a package the plan brings in or moves
/// gives up those checkouts (the whole repo, once none is left), as `compose` drops only the
/// checkouts an added package claims.
fn replace_colliding(
    chosen: &mut BTreeMap<PkgId, Entry>,
    replaced: &mut Vec<(Replaced, PkgId, Vec<String>, bool)>,
) {
    let mut newcomers: Vec<(usize, PkgId)> =
        chosen.iter().filter(|(_, e)| e.changed()).map(|(id, e)| (e.order, id.clone())).collect();
    newcomers.sort();
    for (_, by) in newcomers {
        let Some(e) = chosen.get(&by) else { continue };
        let new: BTreeSet<String> = e.enabled.iter().filter(|t| e.thorn_is_new(t)).cloned().collect();
        let bases: Vec<PkgId> = (chosen.iter())
            .filter(|(id, b)| *id != &by && b.base_repo().is_some())
            .filter(|(_, b)| b.enabled.iter().any(|t| contains(&new, t)))
            .map(|(id, _)| id.clone())
            .collect();
        for id in bases {
            let b = chosen.get_mut(&id).expect("listed above");
            let repo = b.base_repo().unwrap_or_default().to_owned();
            let taken: Vec<String> = b.enabled.iter().filter(|t| contains(&new, t)).cloned().collect();
            b.enabled.retain(|t| !contains(&new, t));
            let thorns = if b.enabled.is_empty() {
                chosen.remove(&id);
                Vec::new()
            } else {
                taken
            };
            replaced.push((Replaced::Base { repo }, by.clone(), thorns, false));
        }
    }
}
