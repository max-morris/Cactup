//! The resolver: named specs plus the installation's present packages → a [`Plan`].
//!
//! A pass seeds the chosen set with the present packages and works off a queue of needs, the
//! named specs first: single-target dependencies to a fixpoint, then any-of dependencies one at a
//! time in a fixed order (dependent, then declaration order), the fixpoint re-run after each.
//! Each need is taken by its target itself, a chosen fork that stands in for it, or a package an
//! override says satisfies it; its range joins the target's constraints, and the target keeps
//! its release if that meets them all, else moves to the newest release that does (when it may
//! move). A release that changes after its dependencies were pushed restarts the pass, keeping
//! what was picked; constraints belong to the release that made them, so a replaced release's
//! never refuse a consistent set. A refusal found during a pass waits for the pass to finish,
//! since a restart may take away the constraint behind it. Passes and steps are bounded.
// Plan §2.2. Greedy: no backtracking into other packages' choices beyond the restarts.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use cactup_ecosystem_common::api::ClaimInfo;
use cactup_ecosystem_common::manifest::ForkOf;
use cactup_ecosystem_common::version::{TrackedReq, TrackedVersion};
use indexmap::IndexSet;

use super::plan::{Choice, ChoiceRule, Library, Placement, Plan, Record, Step, Untrusted};
use super::refusal::{Live, Refusal, Unmet};
use super::{conflict, At, Facts, Fit, PkgId, Present, Release, Req, Request, Requirements, Rules, Why};
use crate::Res;
use anyhow::bail;

/// Passes (restarts) before "no consistent set".
pub(super) const MAX_PASSES: usize = 32;
/// Needs and decisions processed over every pass before "no consistent set".
pub(super) const MAX_STEPS: usize = 50_000;

/// Resolves `request` against the registry `facts` gives, checking CCL requirements with `ccl`.
/// A refusal comes back as a [`Refusal`] inside the error (downcast to read it); anything else
/// is a failure of `facts` or an interrupt.
pub fn resolve(request: &Request, facts: &mut dyn Facts, ccl: &dyn Requirements) -> Res<Plan> {
    let mut present: Vec<Present> = request.present.to_vec();
    for p in &mut present {
        request.rules.apply(&mut p.release);
    }
    present.sort_by(|a, b| a.release.id.cmp(&b.release.id));
    let mut r = Resolver {
        request,
        present,
        cat: Catalog::new(facts, request.rules),
        ccl,
        picks: BTreeMap::new(),
        steps: 0,
        restarted: BTreeSet::new(),
        s: Pass::default(),
    };
    r.cat.ensure(request.specs.iter().map(|s| s.id.clone()))?;
    let mut passes = 0;
    loop {
        passes += 1;
        if passes > MAX_PASSES {
            return Err(r.no_consistent_set().into());
        }
        match r.pass()? {
            Outcome::Done => break,
            Outcome::Restart(t) => {
                r.restarted.insert(t);
            }
        }
    }
    r.finish()
}

/// The registry as the resolver sees it: releases with the rules applied, loaded once.
struct Catalog<'a> {
    facts: &'a mut dyn Facts,
    rules: &'a Rules,
    /// `None`: the Cactarium has no such package.
    loaded: BTreeMap<PkgId, Option<Vec<Release>>>,
    /// Snapshots read, by package and what was asked (a commit or a tip).
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
        // A commit asked for must be what came back.
        if cactup_ecosystem_common::slug::is_commit_prefix(at) && !release.commit.starts_with(at) {
            let (label, commit) = (release.label(), &release.commit);
            bail!("Asked the Cactarium for {id}#{at}, and it answered with {label} at commit {commit}");
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
}

enum Outcome {
    Done,
    /// This package's release changed after its dependencies were pushed.
    Restart(PkgId),
}

/// Where a constraint comes from: the key it is dropped by.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Source {
    Named,
    UpgradeAll,
    /// A release's claim: `label` is `owner/name@version`.
    Release { label: String, id: PkgId },
}

#[derive(Clone, Debug)]
struct Constraint {
    source: Source,
    req: Req,
    thorns: Vec<String>,
    /// From a present package that is not changing: it narrows what a moving package may take,
    /// but a package that may not move is not refused over it (that predates this command).
    passive: bool,
}

#[derive(Clone, Debug)]
enum Thorns {
    All,
    Some(Vec<String>),
}

#[derive(Clone, Debug)]
struct Need {
    target: PkgId,
    req: Req,
    thorns: Thorns,
    why: Why,
    source: Source,
}

#[derive(Clone, Debug)]
struct AnyOf {
    claim: ClaimInfo,
    dependent: PkgId,
    label: String,
}

/// A package in the chosen set.
#[derive(Clone, Debug)]
pub(super) struct Entry {
    pub(super) release: Release,
    pub(super) enabled: BTreeSet<String>,
    /// What the installation has; `None` for a package the plan brings in.
    pub(super) present: Option<Present>,
    /// When the pass first touched it (`usize::MAX`: never): the order of the steps.
    pub(super) order: usize,
    why: Option<Why>,
    /// Its dependencies were pushed in this pass.
    expanded: bool,
    /// Every thorn of whichever release it ends up at.
    whole: bool,
    keep: Vec<String>,
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
}

/// One pass's state, thrown away on a restart.
#[derive(Default)]
struct Pass {
    chosen: BTreeMap<PkgId, Entry>,
    next_order: usize,
    constraints: BTreeMap<PkgId, Vec<Constraint>>,
    singles: VecDeque<Need>,
    any_of: BTreeMap<(PkgId, usize), AnyOf>,
    /// Claims already pushed: (dependent, index of the claim).
    pushed: BTreeSet<(PkgId, usize)>,
    /// Base repos dropped: (repo, the package taking its place, flesh).
    replaced: Vec<(String, PkgId, bool)>,
    failure: Option<Refusal>,
    notes: IndexSet<String>,
    choices: Vec<Choice>,
    brought_by: BTreeMap<PkgId, IndexSet<String>>,
    named: BTreeSet<PkgId>,
    touched: BTreeSet<PkgId>,
    /// Moving packages that already took their newest release in this pass.
    settled: BTreeSet<PkgId>,
}

struct Resolver<'a> {
    request: &'a Request<'a>,
    present: Vec<Present>,
    cat: Catalog<'a>,
    ccl: &'a dyn Requirements,
    /// Releases decided on across restarts: preferred when they still fit.
    picks: BTreeMap<PkgId, Release>,
    steps: usize,
    /// Packages whose change restarted a pass.
    restarted: BTreeSet<PkgId>,
    s: Pass,
}

/// Who takes a need.
enum Identity {
    Itself,
    Other { taker: PkgId, note: String },
    NotStandIn { fork: PkgId },
}

/// The dependency as people read it: `et/carpet@^1.2`, or `any of et/carpet, et/cactuspugh`.
pub(super) fn claim_label(c: &ClaimInfo) -> String {
    let one = |target: &str, range: Option<&str>, sha: Option<&str>| match (range, sha) {
        (_, Some(sha)) => format!("{target}#{sha}"),
        (Some(r), None) => format!("{target}@{r}"),
        (None, None) => target.to_owned(),
    };
    if c.alternatives.is_empty() {
        one(&c.target, c.range.as_deref(), c.sha.as_deref())
    } else {
        let alts: Vec<String> =
            c.alternatives.iter().map(|a| one(&a.target, a.range.as_deref(), a.sha.as_deref())).collect();
        format!("any of {}", alts.join(", "))
    }
}

/// Whether a dependency applies to these thorns: `needed-by` names one of them, or (when the
/// package first enters) it names none.
fn applies(c: &ClaimInfo, scope: &BTreeSet<String>, first: bool) -> bool {
    if c.needed_by.is_empty() {
        return first;
    }
    c.needed_by.iter().any(|n| scope.iter().any(|t| t.eq_ignore_ascii_case(n)))
}

fn contains(set: &BTreeSet<String>, name: &str) -> bool {
    set.iter().any(|t| t.eq_ignore_ascii_case(name))
}

/// Newest first: versions by semver (within a range, tracks are the range's to choose), above
/// any release at no version.
fn newer(a: &Release, b: &Release) -> bool {
    match (&a.at, &b.at) {
        (At::Version(x), At::Version(y)) => x.version > y.version,
        (At::Version(_), _) => true,
        _ => false,
    }
}

impl Resolver<'_> {
    fn fail(&mut self, r: Refusal) {
        if self.s.failure.is_none() {
            self.s.failure = Some(r);
        }
    }

    fn pass(&mut self) -> Res<Outcome> {
        self.s = Pass::default();
        let no_deps = self.request.options.no_deps;
        for p in &self.present {
            let id = p.release.id.clone();
            if !no_deps {
                let label = p.release.label();
                for c in p.release.claims.iter().filter(|c| c.kind == "dependency") {
                    if c.into.is_some() || !c.alternatives.is_empty() || !applies(c, &p.enabled, true) {
                        continue;
                    }
                    let (Ok(target), Ok(req)) =
                        (PkgId::parse(&c.target), Req::from_claim(c.range.as_deref(), c.sha.as_deref()))
                    else {
                        continue;
                    };
                    let source = Source::Release { label: label.clone(), id: id.clone() };
                    let c = Constraint { source, req, thorns: c.needs.clone(), passive: true };
                    self.s.constraints.entry(target).or_default().push(c);
                }
            }
            let entry = Entry {
                release: p.release.clone(),
                enabled: p.enabled.clone(),
                present: Some(p.clone()),
                order: usize::MAX,
                why: None,
                expanded: false,
                whole: false,
                keep: Vec::new(),
            };
            self.s.chosen.insert(id, entry);
        }
        for spec in self.request.specs {
            self.s.named.insert(spec.id.clone());
            let thorns = spec.thorns.clone().map_or(Thorns::All, Thorns::Some);
            let (target, req) = (spec.id.clone(), spec.req.clone());
            let need = Need { target, req, thorns, why: Why::Named, source: Source::Named };
            self.s.singles.push_back(need);
        }
        if self.request.options.upgrade_all {
            for p in &self.present {
                let At::Version(v) = &p.release.at else { continue };
                // Any version on the track it is on.
                let mut req = TrackedReq::any();
                req.alts[0].track = v.track.clone();
                let thorns = Thorns::Some(p.enabled.iter().cloned().collect());
                let target = p.release.id.clone();
                let (req, why, source) = (Req::Range(req), Why::UpgradeAll, Source::UpgradeAll);
                let need = Need { target, req, thorns, why, source };
                self.s.singles.push_back(need);
            }
        }
        loop {
            if gix::interrupt::is_triggered() {
                bail!("Interrupted");
            }
            self.steps += 1;
            if self.steps > MAX_STEPS {
                return Err(self.no_consistent_set().into());
            }
            if let Some(need) = self.s.singles.pop_front() {
                self.load_wave(&need)?;
                if let Some(t) = self.process(need)? {
                    return Ok(Outcome::Restart(t));
                }
            } else if let Some(key) = self.s.any_of.keys().next().cloned() {
                let a = self.s.any_of.remove(&key).expect("the key was just read");
                if let Some(t) = self.decide(a)? {
                    return Ok(Outcome::Restart(t));
                }
            } else {
                break;
            }
        }
        if let Some(f) = self.s.failure.take() {
            return Err(f.into());
        }
        Ok(Outcome::Done)
    }

    /// Loads, in one wave, the need's target and every target still queued.
    fn load_wave(&mut self, need: &Need) -> Res<()> {
        let mut ids = vec![need.target.clone()];
        ids.extend(self.s.singles.iter().map(|n| n.target.clone()));
        for a in self.s.any_of.values() {
            ids.extend(a.claim.alternatives.iter().filter_map(|alt| PkgId::parse(&alt.target).ok()));
        }
        self.cat.ensure(ids)
    }

    fn live(&self, target: &PkgId, c: &Constraint) -> Live {
        let (source, ignore) = match &c.source {
            Source::Named => ("the command line".to_owned(), None),
            Source::UpgradeAll => ("--upgrade-all".to_owned(), None),
            Source::Release { label, id } => {
                let source = if c.passive { format!("installed {label}") } else { label.clone() };
                (source, Some(format!("{id}:dependency:{target}")))
            }
        };
        Live { target: target.clone(), req: c.req.to_string(), source, ignore }
    }

    fn lives(&self, target: &PkgId) -> Vec<Live> {
        let mut out: Vec<Live> = Vec::new();
        for c in self.s.constraints.get(target).into_iter().flatten() {
            let l = self.live(target, c);
            if !out.contains(&l) {
                out.push(l);
            }
        }
        out
    }

    fn no_consistent_set(&self) -> Refusal {
        let constraints = self.restarted.iter().flat_map(|t| self.lives(t)).collect();
        Refusal::NoConsistentSet { constraints }
    }

    fn no_release(&self, t: &PkgId) -> Refusal {
        if self.cat.is_flesh(t) {
            Refusal::FleshUnmet { target: t.clone(), constraints: self.lives(t) }
        } else {
            Refusal::NoReleaseFits { target: t.clone(), constraints: self.lives(t) }
        }
    }

    fn need_live(&self, need: &Need) -> Live {
        let (source, req) = (need.source.clone(), need.req.clone());
        let c = Constraint { source, req, thorns: Vec::new(), passive: false };
        self.live(&need.target, &c)
    }

    /// Whether a package may move to another release: named, touched under `--upgrade`, any
    /// present package under `--upgrade-all`, a flesh (an unmet flesh dependency plans the
    /// flesh's replacement), or brought in by the plan. Never a pseudo-package.
    fn movable(&self, t: &PkgId) -> bool {
        let Some(e) = self.s.chosen.get(t) else { return true };
        if t.is_repo() {
            return false;
        }
        e.present.is_none()
            || self.s.named.contains(t)
            || self.request.options.upgrade_all
            || (self.request.options.upgrade && self.s.touched.contains(t))
            || e.release.flesh
    }

    /// Whether `r` meets the constraints on `t`'s versions (`active_only`: leaving out those of
    /// present packages that are not changing), and has their thorns.
    fn fits(&self, t: &PkgId, r: &Release, active_only: bool) -> (bool, Vec<String>) {
        let mut version_ok = true;
        let mut missing: Vec<String> = Vec::new();
        for c in self.s.constraints.get(t).into_iter().flatten() {
            if active_only && c.passive {
                continue;
            }
            if c.req.fit(r) == Fit::No {
                version_ok = false;
            }
            for th in &c.thorns {
                if r.thorn(th).is_none() && !missing.iter().any(|m| m.eq_ignore_ascii_case(th)) {
                    missing.push(th.clone());
                }
            }
        }
        (version_ok, missing)
    }

    /// The newest release of `t` meeting every constraint on it (and `extra`), with every thorn
    /// they name (and, for a present package, every thorn it has enabled): approved releases
    /// first, one awaiting review only when none fits; a yanked one only for an exact constraint;
    /// never an invalid one. A release picked before a restart wins while it still fits.
    fn select(&mut self, t: &PkgId, extra: Option<&Req>) -> Res<Option<Release>> {
        let constraints: Vec<&Constraint> = self.s.constraints.get(t).into_iter().flatten().collect();
        let mut reqs: Vec<Req> = constraints.iter().map(|c| c.req.clone()).collect();
        reqs.extend(extra.cloned());
        let mut required: Vec<String> =
            constraints.iter().flat_map(|c| c.thorns.iter().cloned()).collect();
        let present = self.s.chosen.get(t).and_then(|e| e.present.clone());
        if let Some(p) = &present {
            required.extend(p.enabled.iter().cloned());
        }
        let mut candidates: Vec<Release> = self.cat.releases(t).to_vec();
        for q in &reqs {
            if let Req::Commit(sha) = q
                && !candidates.iter().any(|r| r.commit.starts_with(sha.as_str()))
                && !t.is_repo()
            {
                let snap = self.cat.snapshot(t, sha)?;
                candidates.push(snap);
            }
        }
        if let Some(p) = &present
            && !candidates.iter().any(|r| r.same(&p.release))
        {
            candidates.push(p.release.clone());
        }
        let present_release = present.as_ref().map(|p| &p.release);
        let ok: Vec<&Release> = candidates
            .iter()
            .filter(|r| r.usable || present_release.is_some_and(|p| p.same(r)))
            .filter(|r| reqs.iter().all(|q| q.fit(r) != Fit::No))
            .filter(|r| required.iter().all(|th| r.thorn(th).is_some()))
            .filter(|r| !r.yanked || reqs.iter().any(|q| q.is_exact() && q.fit(r) == Fit::Yes))
            .collect();
        if let Some(p) = self.picks.get(t)
            && ok.iter().any(|r| r.same(p))
        {
            return Ok(Some(p.clone()));
        }
        let approved: Vec<&Release> = ok.iter().copied().filter(|r| !r.pending).collect();
        let pool = if approved.is_empty() { ok } else { approved };
        let mut best: Option<&Release> = None;
        for r in pool {
            if best.is_none_or(|b| newer(r, b)) {
                best = Some(r);
            }
        }
        Ok(best.cloned())
    }

    /// Who takes a need for `t`: `t` itself when chosen, a package an override says satisfies it,
    /// a chosen fork that stands in for the versions asked, or (to be brought in) `t` itself.
    fn identity(&self, t: &PkgId, req: &Req) -> Identity {
        if self.s.chosen.contains_key(t) {
            return Identity::Itself;
        }
        for x in self.request.rules.satisfiers(t) {
            if self.s.chosen.contains_key(x) {
                let note = format!("{x} stands in for {t} (a claims override)");
                return Identity::Other { taker: x.clone(), note };
            }
        }
        for (id, e) in &self.s.chosen {
            for c in e.release.claims.iter().filter(|c| c.kind == "fork-of" && c.target == t.as_str()) {
                let target = match (&c.range, &c.sha) {
                    (_, Some(sha)) => format!("{}#{sha}", c.target),
                    (Some(r), None) => format!("{}@{r}", c.target),
                    (None, None) => c.target.clone(),
                };
                let Ok(target) = target.parse() else { continue };
                let fork = ForkOf { target, note: None };
                let published = self.cat.published(t);
                let stands_in = t.slug(req).is_some_and(|dep| fork.stands_in_for(&dep, &published));
                if stands_in {
                    let note = format!("{} stands in for {t}", e.release.label());
                    return Identity::Other { taker: id.clone(), note };
                }
                return Identity::NotStandIn { fork: id.clone() };
            }
        }
        Identity::Itself
    }

    fn touch(&mut self, t: &PkgId, why: &Why) {
        let next = self.s.next_order;
        if let Some(e) = self.s.chosen.get_mut(t)
            && e.order == usize::MAX
        {
            e.order = next;
            e.why = Some(why.clone());
            self.s.next_order += 1;
        }
    }

    fn source_label(source: &Source) -> Option<String> {
        match source {
            Source::Release { label, .. } => Some(label.clone()),
            _ => None,
        }
    }

    /// One need. `Some(t)`: `t`'s release changed after its dependencies were pushed.
    fn process(&mut self, mut need: Need) -> Res<Option<PkgId>> {
        let t = need.target.clone();
        if let Req::Tip(tip) = &need.req {
            if t.is_repo() || !self.cat.known(&t) {
                self.fail(Refusal::NotRegistered { target: t, by: None });
                return Ok(None);
            }
            let snap = self.cat.snapshot(&t, tip)?;
            need.req = Req::Commit(snap.commit);
        }
        match self.identity(&t, &need.req) {
            Identity::Itself => {}
            Identity::Other { taker, note } => {
                self.take_by(&taker, &need, note);
                return Ok(None);
            }
            Identity::NotStandIn { fork } => {
                // An unmet flesh dependency plans the flesh's replacement instead.
                if !self.cat.is_flesh(&t) {
                    let fork_label = self.s.chosen[&fork].release.label();
                    let live = self.need_live(&need);
                    self.fail(Refusal::ForkDoesNotStandIn { fork, fork_label, live });
                    return Ok(None);
                }
            }
        }
        if !self.s.chosen.contains_key(&t) && !self.cat.known(&t) {
            let by = (need.source != Source::Named).then(|| self.need_live(&need));
            self.fail(Refusal::NotRegistered { target: t, by });
            return Ok(None);
        }
        let thorns = match &need.thorns {
            Thorns::Some(v) => v.clone(),
            Thorns::All => Vec::new(),
        };
        let (source, req) = (need.source.clone(), need.req.clone());
        let c = Constraint { source, req, thorns, passive: false };
        self.s.constraints.entry(t.clone()).or_default().push(c);
        if matches!(need.source, Source::Release { .. }) {
            self.s.touched.insert(t.clone());
        }
        self.touch(&t, &need.why);
        if let Some(e) = self.s.chosen.get(&t) {
            let (current, is_present) = (e.release.clone(), e.present.is_some());
            let movable = self.movable(&t);
            let (version_ok, missing) = self.fits(&t, &current, !movable);
            let fits = version_ok && missing.is_empty();
            let options = self.request.options;
            let wants_newest = movable
                && is_present
                && !current.flesh
                && !self.s.settled.contains(&t)
                && (self.s.named.contains(&t) || options.upgrade || options.upgrade_all);
            if wants_newest || (movable && !fits) {
                self.s.settled.insert(t.clone());
                match self.select(&t, None)? {
                    Some(new) if !new.same(&current) => {
                        if let Some(restart) = self.change(&t, new) {
                            return Ok(Some(restart));
                        }
                    }
                    Some(_) => {}
                    None if fits => {}
                    None => {
                        let r = self.no_release(&t);
                        self.fail(r);
                        return Ok(None);
                    }
                }
            } else if !fits {
                let installed = current.label();
                let live = self.need_live(&need);
                if version_ok {
                    self.fail(Refusal::PresentLacksThorns { installed, thorns: missing, live });
                } else {
                    self.fail(Refusal::PresentOutsideRange { installed, live });
                }
                return Ok(None);
            }
        } else {
            let Some(release) = self.select(&t, None)? else {
                let r = self.no_release(&t);
                self.fail(r);
                return Ok(None);
            };
            if release.library {
                if need.source == Source::Named {
                    self.fail(Refusal::NamedLibrary { target: t });
                } else {
                    let by = Self::source_label(&need.source).unwrap_or_default();
                    let ignore = format!("{}:dependency:{t}", by.split(['@', '#']).next().unwrap_or(""));
                    let detail =
                        format!("{t} is a library, and the dependency does not say where it goes (into)");
                    self.fail(Refusal::BadClaim { by, claim: t.to_string(), detail, ignore });
                }
                return Ok(None);
            }
            if release.flesh {
                self.replace_flesh(&t);
            }
            let entry = Entry {
                release,
                enabled: BTreeSet::new(),
                present: None,
                order: self.s.next_order,
                why: Some(need.why.clone()),
                expanded: false,
                whole: false,
                keep: Vec::new(),
            };
            self.s.next_order += 1;
            self.s.chosen.insert(t.clone(), entry);
        }
        self.enable(&t, &need, None);
        Ok(None)
    }

    /// Enables a need's thorns in `taker` (the target itself, or whoever stands in for it), and
    /// pushes the dependencies the newly enabled thorns bring.
    fn enable(&mut self, taker: &PkgId, need: &Need, note: Option<String>) {
        let e = &self.s.chosen[taker];
        let names: Vec<String> = match &need.thorns {
            Thorns::All => e.release.thorns.iter().map(|t| t.thorn.clone()).collect(),
            Thorns::Some(list) => {
                let mut names = Vec::new();
                let mut missing = Vec::new();
                for want in list {
                    match e.release.thorn(want) {
                        Some(t) => names.push(t.thorn.clone()),
                        None => missing.push(want.clone()),
                    }
                }
                if !missing.is_empty() {
                    let (taker, live) = (e.release.label(), self.need_live(need));
                    self.fail(Refusal::MissingThorns { taker, thorns: missing, live });
                    return;
                }
                names
            }
        };
        let e = self.s.chosen.get_mut(taker).expect("the taker is chosen");
        if matches!(need.thorns, Thorns::All) && taker == &need.target {
            e.whole = true;
        }
        let newly: BTreeSet<String> = names.into_iter().filter(|n| !contains(&e.enabled, n)).collect();
        e.enabled.extend(newly.iter().cloned());
        let first = !e.expanded && e.is_new();
        if let Some(note) = note {
            if e.present.is_some() && !e.is_new() {
                if !e.keep.contains(&note) {
                    e.keep.push(note);
                }
            } else {
                self.s.notes.insert(note);
            }
        }
        if let Some(label) = Self::source_label(&need.source) {
            self.s.brought_by.entry(taker.clone()).or_default().insert(label);
        }
        if !newly.is_empty() || first {
            self.expand(taker, &newly, first);
        }
    }

    /// A need taken by a fork or an override's stand-in.
    fn take_by(&mut self, taker: &PkgId, need: &Need, note: String) {
        self.touch(taker, &need.why);
        self.enable(taker, need, Some(note));
    }

    /// Moves `t` to `new`. `Some(t)` when its dependencies were pushed already (restart).
    fn change(&mut self, t: &PkgId, new: Release) -> Option<PkgId> {
        self.picks.insert(t.clone(), new.clone());
        let e = self.s.chosen.get_mut(t).expect("a chosen package changes");
        if e.expanded {
            return Some(t.clone());
        }
        let old = std::mem::replace(&mut e.release, new);
        // The upgrade carries the enabled thorns (select made sure the new release has them).
        e.enabled = if e.whole {
            e.release.thorn_names()
        } else {
            e.enabled.iter().filter_map(|n| e.release.thorn(n).map(|t| t.thorn.clone())).collect()
        };
        let old_label = old.label();
        for cs in self.s.constraints.values_mut() {
            cs.retain(|c| !matches!(&c.source, Source::Release { label, .. } if label == &old_label));
        }
        let flesh = e.release.flesh;
        if flesh {
            self.replace_flesh(t);
        }
        None
    }

    /// `t`, a flesh, comes in: every other flesh leaves the chosen set (the base's is replaced).
    fn replace_flesh(&mut self, t: &PkgId) {
        let others: Vec<PkgId> = (self.s.chosen.iter())
            .filter(|(id, e)| *id != t && e.release.flesh)
            .map(|(id, _)| id.clone())
            .collect();
        for id in others {
            let e = self.s.chosen.remove(&id).expect("listed above");
            match e.present.map(|p| p.origin) {
                Some(super::Origin::Base { repo }) => self.s.replaced.push((repo, t.clone(), true)),
                // Another flesh the plan brings in, or (once P3b lets `add` install one) an added
                // flesh package: two fleshes, which no installation can have.
                _ => {
                    let r = Refusal::FleshUnmet { target: t.clone(), constraints: self.lives(t) };
                    self.fail(r);
                }
            }
        }
    }

    /// Pushes `t`'s dependencies that the thorns enabled now bring (all of its enabled thorns
    /// when its release first enters). Libraries are placement, never needs.
    fn expand(&mut self, t: &PkgId, newly: &BTreeSet<String>, first: bool) {
        if self.request.options.no_deps {
            return;
        }
        let e = self.s.chosen.get_mut(t).expect("a chosen package expands");
        e.expanded = true;
        let release = e.release.clone();
        let scope = if first { e.enabled.clone() } else { newly.clone() };
        let label = release.label();
        for (i, c) in release.claims.iter().enumerate() {
            if c.kind != "dependency" || c.into.is_some() || !applies(c, &scope, first) {
                continue;
            }
            if !self.s.pushed.insert((t.clone(), i)) {
                continue;
            }
            let ignore = format!("{t}:dependency:{}", c.target);
            if !c.alternatives.is_empty() {
                let a = AnyOf { claim: c.clone(), dependent: t.clone(), label: label.clone() };
                self.s.any_of.insert((t.clone(), i), a);
                continue;
            }
            let target = match PkgId::parse(&c.target) {
                Ok(target) => target,
                Err(e) => {
                    let (by, claim, detail) = (label.clone(), claim_label(c), e.to_string());
                    self.fail(Refusal::BadClaim { by, claim, detail, ignore });
                    continue;
                }
            };
            let req = match Req::from_claim(c.range.as_deref(), c.sha.as_deref()) {
                Ok(req) => req,
                Err(detail) => {
                    let (by, claim) = (label.clone(), claim_label(c));
                    self.fail(Refusal::BadClaim { by, claim, detail, ignore });
                    continue;
                }
            };
            let thorns = if c.needs.is_empty() { Thorns::All } else { Thorns::Some(c.needs.clone()) };
            let why = Why::Needed {
                by: label.clone(),
                by_id: t.clone(),
                claim: claim_label(c),
                req: req.to_string(),
                needed_by: c.needed_by.clone(),
                rule: None,
            };
            let source = Source::Release { label: label.clone(), id: t.clone() };
            self.s.singles.push_back(Need { target, req, thorns, why, source });
        }
    }

    /// Decides one any-of dependency: an alternative present; else one already in the plan; else
    /// the first listed that conflicts with nothing present or planned; else the first listed.
    fn decide(&mut self, a: AnyOf) -> Res<Option<PkgId>> {
        let label = claim_label(&a.claim);
        let ignore = format!("{}:dependency:{}", a.dependent, a.claim.target);
        let mut alts: Vec<(PkgId, Req, Thorns)> = Vec::new();
        for alt in &a.claim.alternatives {
            let parsed = PkgId::parse(&alt.target)
                .map_err(|e| e.to_string())
                .and_then(|id| Ok((id, Req::from_claim(alt.range.as_deref(), alt.sha.as_deref())?)));
            match parsed {
                Ok((id, req)) => {
                    let thorns =
                        if alt.needs.is_empty() { Thorns::All } else { Thorns::Some(alt.needs.clone()) };
                    alts.push((id, req, thorns));
                }
                Err(detail) => {
                    let by = a.label.clone();
                    self.fail(Refusal::BadClaim { by, claim: label, detail, ignore });
                    return Ok(None);
                }
            }
        }
        self.cat.ensure(alts.iter().map(|(id, _, _)| id.clone()))?;
        let held = |me: &Self, (id, req, _): &(PkgId, Req, Thorns), present: bool| {
            let taker = match me.identity(id, req) {
                Identity::Itself => id.clone(),
                Identity::Other { taker, .. } => taker,
                Identity::NotStandIn { .. } => return false,
            };
            me.s.chosen.get(&taker).is_some_and(|e| e.present.is_some() == present)
        };
        let present = alts.iter().position(|alt| held(self, alt, true));
        let mut pick = present.map(|i| (i, ChoiceRule::Present));
        if pick.is_none() {
            let planned = alts.iter().position(|alt| held(self, alt, false));
            pick = planned.map(|i| (i, ChoiceRule::Planned));
        }
        if pick.is_none() {
            for (i, alt) in alts.iter().enumerate() {
                if !self.conflicts(alt)? {
                    let rule =
                        if i == 0 { ChoiceRule::FirstListed } else { ChoiceRule::FirstWithoutConflict };
                    pick = Some((i, rule));
                    break;
                }
            }
        }
        let (i, rule) = pick.unwrap_or((0, ChoiceRule::FirstListed));
        let (target, req, thorns) = alts.swap_remove(i);
        let (dependent, chosen) = (a.label.clone(), target.clone());
        self.s.choices.push(Choice { dependent, label: label.clone(), chosen, rule });
        let why = Why::Needed {
            by: a.label.clone(),
            by_id: a.dependent.clone(),
            claim: label,
            req: req.to_string(),
            needed_by: a.claim.needed_by.clone(),
            rule: Some(rule),
        };
        let source = Source::Release { label: a.label, id: a.dependent };
        self.s.singles.push_back(Need { target, req, thorns, why, source });
        Ok(None)
    }

    /// Whether an any-of alternative would conflict with what is present or planned: no release
    /// of it fits, a thorn it needs is enabled in another package, an `incompatible-with` either
    /// way, or a fork relation either way.
    fn conflicts(&mut self, (id, req, thorns): &(PkgId, Req, Thorns)) -> Res<bool> {
        if !self.cat.known(id) {
            return Ok(true);
        }
        let Some(candidate) = self.select(id, Some(req))? else { return Ok(true) };
        let names: Vec<String> = match thorns {
            Thorns::All => candidate.thorns.iter().map(|t| t.thorn.clone()).collect(),
            Thorns::Some(v) => v.clone(),
        };
        for (other, e) in &self.s.chosen {
            if other == id {
                continue;
            }
            if names.iter().any(|n| contains(&e.enabled, n)) {
                return Ok(true);
            }
            let incompatible = |a, b| conflict::incompatible(a, b);
            if incompatible(&candidate, &e.release) || incompatible(&e.release, &candidate) {
                return Ok(true);
            }
            let fork_of = |r: &Release, upstream: &PkgId| {
                r.claims.iter().any(|c| c.kind == "fork-of" && c.target == upstream.as_str())
            };
            if fork_of(&candidate, other) || fork_of(&e.release, id) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// After the last pass: replacements, conflicts, libraries, the CCL check, then the plan.
    fn finish(mut self) -> Res<Plan> {
        if self.request.options.replace {
            self.replace_colliding();
        }
        if let Some(r) = conflict::conflicts(&self.s.chosen) {
            return Err(r.into());
        }
        let libraries = self.place_libraries()?;
        self.check_requirements()?;
        // A release at no version is taken to meet a range, with a note.
        let mut entries: Vec<(&PkgId, &Entry)> =
            self.s.chosen.iter().filter(|(_, e)| e.order != usize::MAX).collect();
        entries.sort_by_key(|(_, e)| e.order);
        let mut notes: IndexSet<String> = IndexSet::new();
        for (id, e) in &entries {
            if e.release.at.version().is_some() {
                continue;
            }
            for c in self.s.constraints.get(*id).into_iter().flatten() {
                if c.passive || !matches!(&c.req, Req::Range(q) if *q != TrackedReq::any()) {
                    continue;
                }
                let l = self.live(id, c);
                notes.insert(format!(
                    "{} is at a commit no version names; taken to meet {} (from {})",
                    e.release.label(),
                    l.req,
                    l.source
                ));
            }
        }
        notes.extend(self.s.notes.iter().cloned());
        let options = self.request.options;
        let mut plan = Plan { libraries, notes: notes.into_iter().collect(), ..Plan::default() };
        for (id, e) in &entries {
            let why = e.why.clone().unwrap_or(Why::Named);
            let enabled: Vec<String> = e.enabled.iter().cloned().collect();
            let mut stepped = true;
            match &e.present {
                None => plan.steps.push(Step::Add { release: e.release.clone(), thorns: enabled, why }),
                Some(p) if e.is_new() => {
                    // A package named, or a flesh, moves without the flag.
                    let unflagged =
                        self.s.named.contains(*id) || e.release.flesh || why == Why::UpgradeAll;
                    let flag = if unflagged {
                        None
                    } else if options.upgrade_all {
                        Some("--upgrade-all")
                    } else if options.upgrade {
                        Some("--upgrade")
                    } else {
                        None
                    };
                    let (from, to) = (p.release.clone(), e.release.clone());
                    plan.steps.push(Step::Upgrade { from, to, thorns: enabled, why, flag });
                    if let super::Origin::Base { repo } = &p.origin {
                        let (base_repo, by, flesh) = (repo.clone(), (*id).clone(), e.release.flesh);
                        plan.steps.push(Step::Replace { base_repo, by, flesh });
                    }
                }
                Some(p) => {
                    let newly: Vec<String> =
                        e.enabled.iter().filter(|t| !contains(&p.enabled, t)).cloned().collect();
                    if newly.is_empty() {
                        stepped = false;
                    } else {
                        let label = e.release.label();
                        let id = (*id).clone();
                        plan.steps.push(Step::AddThorns { id, label, thorns: newly, why });
                    }
                }
            }
            for (repo, by, flesh) in &self.s.replaced {
                if by == *id {
                    let (base_repo, by, flesh) = (repo.clone(), by.clone(), *flesh);
                    plan.steps.push(Step::Replace { base_repo, by, flesh });
                }
            }
            for note in &e.keep {
                plan.steps.push(Step::Keep { id: (*id).clone(), note: note.clone() });
            }
            if stepped {
                let choice = self.s.choices.iter().find(|c| &c.chosen == *id).map(ToString::to_string);
                let by =
                    self.s.brought_by.get(*id).map(|b| b.iter().cloned().collect()).unwrap_or_default();
                let (id, requested) = ((*id).clone(), self.s.named.contains(*id));
                plan.records.push(Record { id, requested, by, choice });
            }
            if e.is_new() && e.release.pending {
                plan.pending.push(e.release.label());
            }
            if e.is_new() && self.cat.untrusted.contains(&((*id).clone(), e.release.commit.clone())) {
                plan.untrusted.push(Untrusted { id: (*id).clone(), commit: e.release.commit.clone() });
            }
        }
        plan.rebuild = plan.steps.iter().any(|s| matches!(s, Step::Replace { flesh: true, .. }));
        plan.choices = self.s.choices.clone();
        Ok(plan)
    }

    /// Under `--replace`, a base repo sharing a thorn name with a package the plan brings in or
    /// moves is dropped in its favor.
    fn replace_colliding(&mut self) {
        let mut newcomers: Vec<(usize, PkgId)> = (self.s.chosen.iter())
            .filter(|(_, e)| e.is_new())
            .map(|(id, e)| (e.order, id.clone()))
            .collect();
        newcomers.sort();
        for (_, by) in newcomers {
            let Some(enabled) = self.s.chosen.get(&by).map(|e| e.enabled.clone()) else { continue };
            let base: Vec<(PkgId, String)> = self
                .s
                .chosen
                .iter()
                .filter(|(id, e)| *id != &by && !e.is_new())
                .filter_map(|(id, e)| {
                    let collides = e.enabled.iter().any(|t| contains(&enabled, t));
                    match e.present.as_ref().map(|p| &p.origin) {
                        Some(super::Origin::Base { repo }) if collides => {
                            Some((id.clone(), repo.clone()))
                        }
                        _ => None,
                    }
                })
                .collect();
            for (id, repo) in base {
                self.s.chosen.remove(&id);
                self.s.replaced.push((repo, by.clone(), false));
            }
        }
    }

    /// Every library the final set places, at one release every placement accepts; a library's
    /// own libraries too. Placement goes on under `--no-deps`.
    fn place_libraries(&mut self) -> Res<Vec<Library>> {
        let mut entries: Vec<&Entry> = self.s.chosen.values().collect();
        entries.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.release.id.cmp(&b.release.id)));
        let roots: Vec<Release> = entries.iter().map(|e| e.release.clone()).collect();
        let mut selected: BTreeMap<PkgId, Release> = BTreeMap::new();
        // Each round reads every placing release (the chosen set, then the libraries selected so
        // far) and selects each library again, until nothing changes.
        for _ in 0..64 {
            let mut placements: BTreeMap<PkgId, Vec<(Placement, Live)>> = BTreeMap::new();
            for r in roots.iter().chain(selected.values()) {
                for c in r.claims.iter().filter(|c| c.kind == "dependency") {
                    let Some(into) = &c.into else { continue };
                    let label = r.label();
                    let ignore = format!("{}:dependency:{}", r.id, c.target);
                    let parsed = PkgId::parse(&c.target)
                        .map_err(|e| e.to_string())
                        .and_then(|id| Ok((id, Req::from_claim(c.range.as_deref(), c.sha.as_deref())?)));
                    let (lib, req) = match parsed {
                        Ok(x) => x,
                        Err(detail) => {
                            let claim = claim_label(c);
                            return Err(Refusal::BadClaim { by: label, claim, detail, ignore }.into());
                        }
                    };
                    let (target, source) = (lib.clone(), label.clone());
                    let live = Live { target, req: req.to_string(), source, ignore: Some(ignore) };
                    let placement = Placement { by: label, into: into.clone(), req };
                    placements.entry(lib).or_default().push((placement, live));
                }
            }
            self.cat.ensure(placements.keys().cloned())?;
            let mut next: BTreeMap<PkgId, Release> = BTreeMap::new();
            for (lib, ps) in &placements {
                let reqs: Vec<&Req> = ps.iter().map(|(p, _)| &p.req).collect();
                let mut candidates: Vec<Release> = self.cat.releases(lib).to_vec();
                for q in &reqs {
                    if let Req::Commit(sha) = q
                        && !candidates.iter().any(|r| r.commit.starts_with(sha.as_str()))
                        && self.cat.known(lib)
                    {
                        candidates.push(self.cat.snapshot(lib, sha)?);
                    }
                }
                let ok: Vec<&Release> = candidates
                    .iter()
                    .filter(|r| r.usable && r.library)
                    .filter(|r| reqs.iter().all(|q| q.fit(r) != Fit::No))
                    .filter(|r| !r.yanked || reqs.iter().any(|q| q.is_exact() && q.fit(r) == Fit::Yes))
                    .collect();
                let approved: Vec<&Release> = ok.iter().copied().filter(|r| !r.pending).collect();
                let pool = if approved.is_empty() { ok } else { approved };
                let mut best: Option<&Release> = None;
                for r in pool {
                    if best.is_none_or(|b| newer(r, b)) {
                        best = Some(r);
                    }
                }
                let lives: Vec<Live> = ps.iter().map(|(_, l)| l.clone()).collect();
                let Some(best) = best else {
                    let target = lib.clone();
                    if !self.cat.known(lib) {
                        let by = lives.into_iter().next();
                        return Err(Refusal::NotRegistered { target, by }.into());
                    }
                    let sources: BTreeSet<&str> = ps.iter().map(|(p, _)| p.by.as_str()).collect();
                    if sources.len() > 1 {
                        return Err(Refusal::LibraryClash { library: target, placements: lives }.into());
                    }
                    return Err(Refusal::NoReleaseFits { target, constraints: lives }.into());
                };
                next.insert(lib.clone(), best.clone());
            }
            let same = next.len() == selected.len()
                && next.iter().zip(&selected).all(|((a, ra), (b, rb))| a == b && ra.same(rb));
            if same {
                let mut out = Vec::new();
                for (lib, release) in selected {
                    let placements = placements.remove(&lib).unwrap_or_default();
                    let placements = placements.into_iter().map(|(p, _)| p).collect();
                    out.push(Library { release, placements });
                }
                return Ok(out);
            }
            selected = next;
        }
        bail!("Libraries placed inside each other without end")
    }

    /// Every hard CCL requirement of every thorn the plan enables is met by the final set;
    /// otherwise a refusal, whatever the number of providers (cactup never fills in a
    /// dependency nobody declared).
    fn check_requirements(&mut self) -> Res<()> {
        let unmet = conflict::unmet_requirements(&self.s.chosen, self.ccl);
        if unmet.is_empty() {
            return Ok(());
        }
        let mut out: Vec<Unmet> = Vec::new();
        let mut asked: BTreeMap<super::Requirement, Vec<super::Provider>> = BTreeMap::new();
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
        Err(Refusal::Unclaimed(out).into())
    }
}
